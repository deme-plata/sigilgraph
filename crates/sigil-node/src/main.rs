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
mod genesis; // 2026-08-23: moved out so sigil-top's `producer` feature shares the REAL build_genesis()
mod mint; // 2026-08-23: moved out so sigil-top's `producer` feature shares the REAL mint_next_block()
mod dag; // 2026-08-23: moved out so sigil-top's `producer` feature shares the REAL braid wiring
// 2026-08-26 (frontier-memo adoption): `frontier.rs` already declared `pub mod
// frontier;` in `lib.rs`; this is the matching bin-local declaration (same
// two-independent-copies-of-one-source pattern as `dag`/`mint`/`genesis` above)
// so this binary can call `frontier::dag_build_frontier_memo` — see the call
// site below and `frontier.rs`'s own module doc for the validation history.
mod frontier;
mod cli;
mod snapshot;
mod rate_governor;
mod ingest;
mod dandelion_relay; // wires sigil-dandelion's Action into real TOPIC_TXS gossip
mod wg_relay; // zero-config WireGuard side-mesh — see its module docs
mod sync_auth;
mod search_index;
mod serve_read; // header-only reads for the backfill SERVE path — see its module doc
mod producer_signing;
mod finality_wire; // Phase 2 finality observer plumbing — zero consensus effect, see its module doc

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
use sigil_narwhal_mempool::MempoolBackend;
use sigil_tx::{apply_tx, ed25519_keygen, ed25519_sign_tx, SignedTx, SigilTx};
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
/// Fail-loud threshold for `Braid::finality_margin()` — a quarter of the
/// `BraidConfig::final_depth` default (512, bumped from 64 on 2026-08-15).
/// Not derived from the live config automatically (the running braid's
/// config isn't retained for later reads) — if `SIGIL_DAG_FINAL_DEPTH` is
/// ever overridden away from the default, revisit this alongside it.
const FINALITY_MARGIN_WARN_THRESHOLD: u64 = 128;

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
    // 2026-08-20: without this, RUST_LOG was completely inert for this binary — no
    // tracing subscriber was ever installed, so every tracing::{info,debug,warn}!
    // call anywhere in the dependency graph (flux-p2p's gossipsub mesh/graft/prune
    // diagnostics included) was silently dropped, on every node including Epsilon.
    // Purely additive: respects RUST_LOG exactly like eprintln! output already did
    // NOT respect it (application logging here uses hardcoded eprintln!, unaffected).
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

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
    // Zero-config WireGuard side-mesh (wg_relay.rs) — additive, never touches
    // this node's primary transport/listen address. Best-effort: a failure
    // here just means no WG side-mesh this session, direct transport is
    // unaffected either way.
    let wg_state = wg_relay::ensure_up(&cfg.db_path, &local_peer_id);
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
                    //
                    // 2026-08-20: `get_by_height` (verifies the real height field),
                    // not `get` (trusts array-position == height, which `open()`'s
                    // startup scan doesn't actually guarantee — see its doc comment
                    // in chain_log.rs). Was `chain_log.get(...)` — that mismatch is
                    // exactly what caused every single restart to report a false
                    // "doesn't match" and fall back to a full replay, even though
                    // the snapshot was never actually corrupt or diverged.
                    let log_tip_hash = chain_log.get_by_height(snap.snapshot_height).map(|b| b.hash());
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
                                // 2026-08-20 (part 2 of the same bug): `chain_log.height()`
                                // is the raw ON-DISK RECORD COUNT, which carries the exact
                                // same +1077 historical anomaly as the lookup above — it is
                                // NOT reliably "real tip height + 1" post-restart. Compare
                                // against `tip_real_height()` (reads the last record's own
                                // `header.height` field) instead, so a healthy snapshot boot
                                // isn't rejected by a unit mismatch (count vs. real height).
                                let log_next_height = chain_log.tip_real_height().map(|h| h + 1);
                                if Some(restored.height()) == log_next_height {
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
                                    eprintln!("⚠ snapshot boot incomplete: chain H={} vs log real-tip-next {:?} (tail read {}, applied {}) — falling back to full replay",
                                        restored.height(), log_next_height, tail_read, tail_applied);
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
            let graw = chain_log::encode_record(&genesis).unwrap_or_default();
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
        // SIGIL_BRAIDPOOL_v1_1.md Phase B: MempoolBackend is the ONE mempool
        // handle this node and sigil-api's money API both hold (see its doc
        // comment for the correctness hazard that requires this). Defaults
        // to the legacy single-mutex backend unless SIGIL_BRAIDPOOL=1 —
        // byte-for-byte the same behavior as before this type existed.
        let mempool: Arc<MempoolBackend> = Arc::new(MempoolBackend::from_env());

        // ONE-CHAIN step 2: the PROPER money API (sigil-api, axum + flux-api SDKs)
        // — balance / supply / signed-send / tx-status. Shares this producer's
        // mempool + a published state snapshot. Supersedes rpcd; gated by
        // SIGIL_MONEY_API=<addr> (e.g. 0.0.0.0:8181).
        // P1 mining-onto-the-braid: the bridge the producer publishes its frontier
        // into and pops verified dual-lane solves from. Always constructed (cheap,
        // inert without a producer); the API exposes it, and `SIGIL_MINING_GATED=1`
        // makes block production wait on real work.
        // Real wallet/transaction/block search (2026-08-20, operator-directed).
        // Loads whatever was already indexed as of last shutdown (fast — no
        // full replay needed on every restart) and spawns a background
        // catch-up + poll task. See search_index.rs's module doc for why this
        // tails ChainLog instead of hooking block-apply directly: it's a
        // completely separate reader of the same durable log, so it can never
        // affect (or be affected by) the actual consensus/settlement code.
        // KILL-SWITCH (2026-08-26, operator-approved during the frozen-tip incident).
        // The indexer is a pure *reader* of ChainLog and can never corrupt consensus
        // state -- but it is NOT free: it shares this process's CPU and its cgroup RSS
        // ceiling. Measured on the live producer that day: the on-disk index was
        // ~210k blocks behind tip (indexed-to-height 1,971,511 vs tip 2,180,884), so
        // every boot re-entered a multi-hour catch-up that pinned a core (perf: 19%
        // num_bigint, 13% serde_json, drop_in_place<flux_search::Document>,
        // SearchEngine::rebuild_runtime_indexes) and grew RSS ~17 MB/s until the
        // cgroup MemoryMax OOM-killed the node -- a restart loop that never reached
        // block production at all (io: 10.4 GB read, 45 KB written).
        //
        // SIGIL_SEARCH_INDEX=0 skips BOTH the ~1.25 GB index load and the background
        // catch-up. `/search` then answers from an empty engine (zero results) instead
        // of taking the whole node down with it. Default is UNCHANGED (enabled), so
        // this is inert unless the operator sets the env var.
        // REVERT: unset SIGIL_SEARCH_INDEX (or set it to 1), restart.
        let search_enabled = std::env::var("SIGIL_SEARCH_INDEX")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let search_engine: Arc<Mutex<flux_search::SearchEngine>> =
            Arc::new(Mutex::new(if search_enabled {
                search_index::load_or_new(&snap_dir)
            } else {
                eprintln!(
                    "🔎 search index DISABLED (SIGIL_SEARCH_INDEX=0) — /search returns \
                     no results; block production unaffected"
                );
                flux_search::SearchEngine::new()
            }));
        if search_enabled {
            search_index::spawn_indexer(
                snap_dir.clone(),
                Arc::clone(&search_engine),
                std::time::Duration::from_secs(5),
            );
        }
        let mining_bridge = Arc::new(sigil_api::mining::MiningBridge::new());
        // Wallet-authenticated send queue — the producer drains it into every
        // block's tx set unconditionally (see the `block_txs` build below),
        // same "always constructed, inert without traffic" shape as
        // `mining_bridge`. See `sigil_api::send` module docs for why this is
        // a bridge instead of routing through `Mempool::ingest`.
        let send_bridge = Arc::new(sigil_api::send::SendBridge::new());
        // SIGIL-Nation wallet-authenticated welfare-claim / citizen-attest
        // queue — same drain contract as `send_bridge`. Inert without
        // traffic, and consensus refuses nation txs below the activation
        // height anyway (sigil_bank::welfare::WELFARE_FROM_HEIGHT).
        let nation_bridge = Arc::new(sigil_api::nation::NationBridge::new());
        // PV-1 private transfers. Drained on the same contract as `send_bridge`:
        // re-embedded into every candidate, retired only when one lands on the spine.
        let shielded_bridge = Arc::new(sigil_api::shielded::ShieldedBridge::new());
        // Dandelion++ tx-gossip privacy relay (dandelion_relay.rs): one actor
        // task owns the DandelionRouter; everything else just sends it a Cmd.
        // Always on — no env gate, since it degrades to normal fluff-only
        // behavior with zero peers or zero traffic, same as the crate's own
        // "originate with no peers falls back to fluff" test guarantees.
        // Must come after `shielded_bridge`: spawn() wires its relay hook.
        let dandelion_tx = crate::dandelion_relay::spawn(Arc::clone(&mgr), Arc::clone(&mempool), Arc::clone(&shielded_bridge));
        // R1: tx/batch INGEST BRIDGE — the first real user-tx path into the producer
        // mempool (wallets / rpcd-forwarder / loadgen). Env-gated by SIGIL_API_PORT;
        // shares the mempool Arc like the TXGEN feeder. Off unless set.
        if let Some(api_port) = std::env::var("SIGIL_API_PORT").ok()
            .and_then(|s| s.parse::<u16>().ok()).filter(|p| *p > 0)
        {
            crate::ingest::spawn(Arc::clone(&mempool), dandelion_tx.clone(), api_port);
            eprintln!("\u{1f310} tx ingest API on :{api_port} — POST /tx, POST /batch, GET /mempool");
        }
        // SIGIL <-> Polygon bridge — admin/relayer are real SIGIL wallet
        // addresses (32-byte Ed25519 pubkeys), NOT Polygon/EVM addresses;
        // configured via env so no key material is ever hardcoded in source.
        // `admin_wallet` can only pause/rotate, never unlock — see bridge
        // module docs for why that split is deliberate. Absent env vars
        // leave the bridge inert (submit_lock/unlock/pause all reject)
        // rather than defaulting to some implicit trusted address.
        let bridge_bridge = Arc::new(sigil_api::bridge::BridgeBridge::new(
            std::env::var("SIGIL_BRIDGE_ADMIN_WALLET").ok().as_deref().and_then(sigil_api::hex32),
            std::env::var("SIGIL_BRIDGE_RELAYER_WALLET").ok().as_deref().and_then(sigil_api::hex32),
        ));
        // The bridge's SHIELDED vault. Since the privacy-only change consensus refuses
        // every transparent `Send`, so a lock is now a `Shield` into a vault-owned note
        // and the vault needs a real key — see `sigil_api::bridge_vault`. Absent seed =
        // no vault = locking refuses loudly; it never falls back to the retired shape.
        {
            use sigil_api::bridge_vault::{BridgeVault, DEFAULT_LEDGER_PATH, DEFAULT_SEED_PATH};
            let seed_path = std::env::var("SIGIL_BRIDGE_VAULT_SEED")
                .unwrap_or_else(|_| DEFAULT_SEED_PATH.to_string());
            let ledger_path = std::env::var("SIGIL_BRIDGE_VAULT_LEDGER")
                .unwrap_or_else(|_| DEFAULT_LEDGER_PATH.to_string());
            match BridgeVault::open(std::path::Path::new(&seed_path), std::path::Path::new(&ledger_path)) {
                Ok(v) => {
                    let pk = v.public_key_hex();
                    bridge_bridge.set_vault(Arc::new(v));
                    println!("🔐 bridge vault loaded — shielded pk {pk} (seed {seed_path})");
                }
                Err(e) => {
                    // Not fatal: a node that does not run the bridge is a normal node.
                    println!("ℹ bridge vault NOT loaded ({seed_path}: {e}) — /v1/bridge/lock will refuse");
                }
            }
        }
        // Wallet-authenticated swap / add-liquidity / remove-liquidity queue —
        // same "always constructed, inert without traffic" shape as
        // `send_bridge`/`bridge_bridge`. See `sigil_api::dex` module docs.
        let dex_bridge = Arc::new(sigil_api::dex::DexBridge::new());
        // Wallet-authenticated USDS mint/redeem queue — same shape as
        // `dex_bridge`. See `sigil_api::usds` / `sigil_usds` module docs.
        let usds_bridge = Arc::new(sigil_api::usds::UsdsBridge::new());
        // USDS <-> Polygon bridge — a SEPARATE instance from `bridge_bridge`
        // (native SIGIL), its own vault/admin/relayer, its own env vars, on
        // purpose (see `sigil_api::usds_bridge` module docs for why the two
        // bridges must never share a trust boundary).
        let usds_polygon_bridge = Arc::new(sigil_api::usds_bridge::UsdsBridgeBridge::new(
            std::env::var("SIGIL_USDS_BRIDGE_ADMIN_WALLET").ok().as_deref().and_then(sigil_api::hex32),
            std::env::var("SIGIL_USDS_BRIDGE_RELAYER_WALLET").ok().as_deref().and_then(sigil_api::hex32),
        ));
        // Read-only GHOSTDAG snapshot for the DagKnight visualization — see
        // sigil_api::dagknight module docs. `braid` (below, once dag_mode is
        // on) is never locked; only the periodic copy written here is.
        // Always constructed, inert until the dag-snapshot tick starts
        // writing (dag_mode off ⇒ stays at its zero-value default forever).
        let dag_snapshot_bridge = Arc::new(sigil_api::dagknight::DagSnapshotBridge::new());
        // Durable hashrate/miner-count time series for the wallet's Network
        // Power modal (24h/7d/30d/1y/all) — see
        // sigil_api::mining_history module docs. Lives in its own subdir of
        // the same snapshot directory search_index already uses; opening it
        // is cheap (flux-db, same as sigil-block-store) and always
        // succeeds/creates-on-first-run, so this is unconditional (not
        // gated on SIGIL_MONEY_API) the same way mining_bridge is — the
        // sampler itself only ever runs once the money API is up, below.
        let mining_history_store = Arc::new(
            sigil_api::mining_history::MiningHistoryStore::open(snap_dir.join("mining-history"))
                .expect("open mining history store"),
        );
        let money_state: Option<Arc<std::sync::RwLock<SigilState>>> =
            std::env::var("SIGIL_MONEY_API").ok().filter(|s| !s.is_empty()).map(|addr| {
                let shared = Arc::new(std::sync::RwLock::new(chain.state_snapshot()));
                let app = sigil_api::AppState {
                    mempool: Arc::clone(&mempool),
                    state: Arc::clone(&shared),
                    mining: Arc::clone(&mining_bridge),
                    send: Arc::clone(&send_bridge),
                    shielded: Arc::clone(&shielded_bridge),
                    bridge: Arc::clone(&bridge_bridge),
                    dex: Arc::clone(&dex_bridge),
                    usds: Arc::clone(&usds_bridge),
                    usds_bridge: Arc::clone(&usds_polygon_bridge),
                    search: Arc::clone(&search_engine),
                    dagknight: Arc::clone(&dag_snapshot_bridge),
                    history: Arc::clone(&mining_history_store),
                    network: Some(Arc::clone(&mgr)),
                    nation: Arc::clone(&nation_bridge),
                };
                // Samples the live mining aggregate once/minute into the
                // durable store above. Same "reader of already-published
                // state, never touches consensus" shape as search_index's
                // indexer — see sigil_api::mining_history::spawn_sampler doc.
                sigil_api::mining_history::spawn_sampler(
                    Arc::clone(&mining_bridge),
                    Arc::clone(&mining_history_store),
                    std::time::Duration::from_secs(60),
                );
                tokio::spawn(async move {
                    if let Err(e) = sigil_api::serve(&addr, app).await {
                        eprintln!("\u{26a0} sigil-api serve failed: {e}");
                    }
                });
                eprintln!("\u{1f4b0} sigil-api money API on {} — /v1/{{balance,supply,transactions,mining/*,bridge/*,pools,swap,add_liquidity,remove_liquidity,usds/*,usds_bridge/*}}", std::env::var("SIGIL_MONEY_API").unwrap_or_default());
                eprintln!("\u{1f309} usds bridge: admin={} relayer={}",
                    usds_polygon_bridge.admin_hex().unwrap_or_else(|| "UNSET".into()),
                    usds_polygon_bridge.relayer_hex().unwrap_or_else(|| "UNSET (locks will be rejected)".into()));
                eprintln!("\u{1f309} bridge: admin={} relayer={}",
                    bridge_bridge.admin_hex().unwrap_or_else(|| "UNSET".into()),
                    bridge_bridge.relayer_hex().unwrap_or_else(|| "UNSET (locks will be rejected)".into()));
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
                        let len = mp.len();
                        if len >= target { 0 } else { target - len }
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
                    mp.ingest(batch);
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
        // QTFT-2: receipt-side topology-commitment verification. Default is
        // OBSERVE ONLY (recompute + count + loudly log a genuine mismatch,
        // never refuse the block) — this is deliberately NOT gated by
        // sigil-header::TOPOLOGY_COMMITMENT_ACTIVATION_HEIGHT, which its own
        // doc comment reserves for a LATER, more formal multi-validator-
        // committee enforcement mechanism this does not presume to be. An
        // operator opts a SPECIFIC node into actually refusing mismatching
        // blocks with SIGIL_TOPOLOGY_ENFORCE=1 — e.g. once >1 real producer
        // makes the invariant non-degenerate and the operator wants to prove
        // rejection works before considering fleet-wide enforcement.
        let topology_enforce = std::env::var("SIGIL_TOPOLOGY_ENFORCE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
        if dag_mode {
            eprintln!("🧶 QTFT topology verification: {} (SIGIL_TOPOLOGY_ENFORCE={})",
                if topology_enforce { "ENFORCING (rejects mismatches)" } else { "observe-only (logs mismatches, never rejects)" },
                if topology_enforce { "1" } else { "0" });
        }
        let mut topology_stats = TopologyStats::default();
        // Blocks admitted live via THIS gossipsub path since boot — the
        // verifier requires a full window's worth of these before ever
        // comparing, so a fresh boot / recent snapshot restore can never look
        // like a peer mismatch just because OUR own window isn't populated
        // yet (see verify_topology_on_receipt's doc comment).
        let mut live_blocks_witnessed: u64 = 0;
        // QTFT Path C (SIGIL_QTFT_TOPOLOGY_v0.md's "knot-routing in p2p" idea,
        // scoped to what's real today): backfill requests currently pick an
        // arbitrary connected peer. This tracks which peer has actually been
        // relaying which producer's strand, so a gap for a SPECIFIC producer
        // can prefer a peer topologically close to it. See PeerProducerAffinity.
        let mut peer_affinity = PeerProducerAffinity::default();
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
        // 2026-08-26 (frontier-memo adoption): the previous tick's built frontier,
        // carried forward so `dag_build_frontier_memo` can extend it instead of
        // `dag_build_frontier`'s full O(window) rebuild every tick (the measured
        // live bottleneck — see `frontier.rs`'s module doc). None until the first
        // tick builds one. Deliberately NOT reset on a braid reseed (`dag_seed_braid`
        // below): the memo function's own `cached.height() >= chain.height()`
        // usability check plus its two reorg fallbacks already force a correct
        // full rebuild the first time it's called against a reseeded braid (a
        // reseed only re-inserts already-settled blocks, so the post-reseed
        // selected tip sits at/below the settled height while a stale cache built
        // ahead of it does not — the empty-path fallback's
        // `frontier.parent_hash() != tip` check catches exactly this), so an
        // explicit reset here would be redundant, not a safety requirement.
        let mut frontier_cache: Option<ChainTip> = None;
        // v7.1.29: which SendBridge-pending tx hashes each of OUR OWN minted
        // candidates carries, keyed by that candidate's own block hash — lets
        // `dag_drain_apply` retire a pending send the instant its containing
        // candidate is confirmed on the settled spine, and leave it untouched
        // (still pending, still retried next tick) if that candidate orphans.
        // Bounded the same way `dag_bodies` is; see `prune_mint_hash_tracking`.
        let mut mint_hash_to_tx_hashes: std::collections::HashMap<BlockHash, Vec<[u8; 32]>> =
            std::collections::HashMap::new();
        // v0 metrics: ordered-but-not-state-applied (off-spine / non-extending /
        // already-applied), refused-below-final, structural rejects, apply fails.
        let (mut dag_ord_skipped, mut dag_below_final, mut dag_rejected, mut dag_apply_failed) =
            (0u64, 0u64, 0u64, 0u64);
        // 2026-08-19 (deep-catchup freeze fix): counts "missing parents" gossip
        // rejections so the request-log print below can be rate-limited by count,
        // not just by the 15ms request-throttle — see the print site for why.
        let mut dag_missing_parents_logged: u64 = 0;
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
        // DagKnight visualization: copy the last 200 finalized blocks + their
        // GHOSTDAG coloring out of `braid` every 5s. Cheap in-memory copy on
        // this same single-threaded loop — never a lock on `braid` itself,
        // see sigil_api::dagknight module docs.
        let mut dag_snapshot_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        dag_snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        // 2026-08-20 (happysrv OOM investigation, part 2): the 15ms request-RATE
        // throttle alone still let up to ~66 gap-backfill fetches/sec spawn
        // concurrently — each one fetches its full (now-bounded, FETCH_CHUNK-size)
        // response into memory BEFORE the bounded `bf_tx` channel's backpressure
        // can apply (that only throttles the .send() into the channel, which
        // happens AFTER the network fetch + deserialize already cost real
        // memory). A deeply-behind node kept climbing straight to the 8G cgroup
        // cap and OOM-killing within ~40s even with each individual request
        // capped — confirmed live (memory graph: ~2G→8.19G in 40s). This
        // semaphore hard-caps how many gap-backfill fetches can be truly
        // in-flight at once, regardless of how fast new "missing parents"
        // events arrive; a permit is held for the lifetime of the spawned
        // task and released automatically (RAII) whether it succeeds, errors,
        // or times out — no separate reset path to get stuck.
        let gap_fetch_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
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
        // Phase 2 of SIGIL True Instant Finality — OBSERVATIONAL ONLY.
        // Inert unless SIGIL_FINALITY_COMMITTEE is set; nothing it returns is
        // read by production, validation, or fork choice. See finality_wire.rs.
        let mut finality = finality_wire::FinalityWire::from_env();
        // Fail-loud finality monitoring (2026-08-15, the P=6 k=1 investigation):
        // `below_final` was already tracked in BraidStats but nothing ever
        // surfaced it to an operator — a node could be silently orphaning
        // legitimate blocks (reordering exceeding final_depth) with zero
        // visible signal beyond a quietly-incrementing counter. Checked on the
        // same 5s heartbeat cadence as everything else in this loop.
        let mut last_below_final: u64 = 0;
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
        // 2026-08-23 — WHY THIS IS BYTES AND NOT A COUNT, and why it is derived
        // rather than configured.
        //
        // These caches were capped by ENTRY COUNT (2048 / 256). An entry is a
        // whole serialized+compressed serve response, and those are NOT small:
        // measured over 482 real responses on this producer, mean 0.9 MB,
        // median 0.3 MB, max 3.3 MB. So "2048 chunks" was really ~1.8 GB
        // typical and ~6.8 GB worst case — two constants that LOOK like a
        // memory bound while bounding nothing.
        //
        // What that cost, live: an archive-syncing peer requests big 32k-header
        // chunks, every one is cached at max size, the process crossed its
        // cgroup MemoryHigh (11 GiB), and the kernel parked it in
        // uninterruptible sleep (`mem_cgroup_handle_over_high`). Result: block
        // production stopped dead, 174 inbound connections sat unaccepted, and
        // every client saw `peers 0`. It looked like a sync bug for hours; it
        // was the cache.
        //
        // OUT OF THE BOX: the budget is derived from THIS process's actual
        // memory ceiling (cgroup v2/v1 limit, else system RAM), so a small VPS
        // and a 64 GB box both get something sane with no operator action and
        // no env var. 8% of the ceiling, clamped to [64 MiB, 2 GiB] — big
        // enough to serve a real archive sync, far too small to threaten the
        // limit. SIGIL_SERVE_CACHE_BYTES overrides it for anyone who wants to.
        let serve_cache_budget: usize = std::env::var("SIGIL_SERVE_CACHE_BYTES")
            .ok().and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or_else(|| {
                let ceiling = detect_memory_ceiling_bytes();
                ((ceiling / 100) * 8).clamp(64 * 1024 * 1024, 2 * 1024 * 1024 * 1024)
            });
        // The hot cache holds near-tip retries; it needs far less room.
        let hot_cache_budget: usize = (serve_cache_budget / 8).max(16 * 1024 * 1024);
        eprintln!("\u{1f9e0} serve caches: {} MiB finalized + {} MiB hot (auto-sized from a {} MiB memory ceiling; override SIGIL_SERVE_CACHE_BYTES)",
            serve_cache_budget / 1048576, hot_cache_budget / 1048576, detect_memory_ceiling_bytes() / 1048576);
        let mut serve_cache_bytes: usize = 0;
        let mut hot_cache_bytes: usize = 0;
        let (mut cache_hits, mut cache_miss): (u64, u64) = (0, 0);
        let mut last_cache_log = std::time::Instant::now();
        // 2026-08-22 (root cause of the multi-hour production stall, found live):
        // the cache above only covers IMMUTABLE ranges (hi < window_base) — a
        // request whose `hi` sits near the live tip is NEVER cacheable, so a
        // peer re-requesting the exact same near-tip range (proven live: the
        // same peer, ~2×/s, identical [lo..=hi] with hi six blocks behind the
        // real tip) forces a full disk read + zstd compress EVERY time, INLINE
        // in the same synchronous loop that drives block production — measured
        // live via the 5s heartbeat firing every 43-69s instead of on schedule.
        // This is safe to cache too, just with a short TTL instead of forever:
        // `hi` is always clamped to `chain.height()-1` (see below), so every
        // height in a served range is already durably committed on THIS node
        // by the time it's served — its content can never retroactively change
        // — the only thing that goes stale is whether a NEWER, wider range has
        // since become available, and a genuinely newer request has a
        // different `hi` and therefore a different key, so it always falls
        // through to a fresh read regardless of this cache. An identical
        // repeat within the TTL — the exact retry-storm pattern that caused
        // the live stall — now gets a cheap hit instead of redoing the work.
        let mut hot_cache: std::collections::HashMap<(u64, u64, bool, u32), (std::sync::Arc<Vec<u8>>, std::time::Instant)> =
            std::collections::HashMap::new();
        let mut hot_cache_order: std::collections::VecDeque<(u64, u64, bool, u32)> =
            std::collections::VecDeque::new();
        let hot_cache_ttl = std::time::Duration::from_millis(
            std::env::var("SIGIL_SERVE_HOT_TTL_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(750),
        );
        let (mut hot_hits, mut hot_miss): (u64, u64) = (0, 0);
        let mut last_hot_cache_log = std::time::Instant::now();
        // Memo for the codec=4 M1 fold trailer's archive_root, keyed by the anchor
        // height it was computed at. See the codec==4 arm for why this exists: that
        // root is a hash over the WHOLE chain's skeleton records, recomputed inline
        // on the produce loop for every single request before 2026-08-26.
        let mut fold_root_memo: Option<(u64, [u8; 32])> = None;
        // Completed off-thread serves, handed back so the loop can cache their
        // bytes. Payload = (cache key, immutable?, hot-eligible?, blob). See the
        // OFF-THREAD SERVE block in the rr-backfill arm for why this exists.
        // Bounded + drained with try_recv: a full channel drops a cache FILL, never
        // a response (the response is sent from the worker, before this).
        #[allow(clippy::type_complexity)]
        let (serve_done_tx, mut serve_done_rx) = tokio::sync::mpsc::channel::<(
            (u64, u64, bool, u32),
            bool,
            bool,
            std::sync::Arc<Vec<u8>>,
        )>(256);
        // Hard ceiling on expensive (cache-miss) backfill compute — see the
        // throttle's own doc comment at its check site below.
        let mut last_expensive_serve = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1)).unwrap_or_else(std::time::Instant::now);
        let mut expensive_throttled: u64 = 0;
        // BATTERIES-INCLUDED SELF-PROFILE (2026-08-23). The producer was minting
        // ~5 blocks/MINUTE against an 8 blocks/SECOND floor and NOTHING in the
        // node could say why. `perf` on the live process returned bare addresses
        // (the release profile stripped the symbol table), so five candidate
        // causes had to be eliminated one at a time by inferring from log rates
        // over hours — memory, the frontier walk, peer starvation, backfill
        // serving — and the real hot path stayed invisible.
        //
        // A node should not need an external profiler, a symbol table, or an SSH
        // session to say where its own tick went. These accumulate real
        // per-phase wall-clock inside the mint loop and print a breakdown every
        // few seconds, always on, no configuration. If minting is slow, the node
        // now TELLS you which phase owns the time.
        // OPT-IN, DEFAULT OFF (2026-08-23). This instrumentation produced the
        // finding that finally explained the producer's collapse — measured
        // 9.8s of a 10s window, 13.1s of 13s, 12.2s of 12s spent in INLINE
        // BACKFILL SERVING, with ZERO mint ticks executed. That is the root
        // cause of ~5 blocks/min against a 37 blocks/s target.
        //
        // But the always-on version STOPPED BLOCK PRODUCTION when deployed
        // (0 blk/60s vs 17 before; rolled back). The ServeTimer guard runs its
        // Drop on every request path including the throttled early-`continue`
        // ones, and on a loop already saturated by serving that made the
        // contention worse. So it is gated: the diagnostic stays available and
        // costs nothing unless explicitly asked for.
        //
        //   SIGIL_MINT_PROFILE=1   → per-phase mint-tick breakdown every 10s
        //
        // Do NOT flip this on a healthy production producer without watching
        // block rate; it is a diagnostic for a node that is ALREADY sick.
        let mint_profile: bool =
            std::env::var("SIGIL_MINT_PROFILE").ok().as_deref() == Some("1");
        let mut ph_frontier_us: u64 = 0;
        let mut ph_mint_us: u64 = 0;
        let mut ph_drain_us: u64 = 0;
        let ph_serve_us = std::sync::atomic::AtomicU64::new(0);
        let mut ph_ticks: u64 = 0;
        let mut last_phase_log = std::time::Instant::now();
        // PER-PEER expensive-serve clock. See the throttle's doc comment at its
        // check site: a single GLOBAL slot let one runaway peer starve every
        // other node's backfill. Bounded: pruned to the most recent peers so a
        // churn of peer ids can't grow this without limit.
        let mut last_expensive_by_peer: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        // In-flight cap for OFF-THREAD FULL-BLOCK serves (2026-08-31). Deep-history
        // full-block backfill used to be the one serve class with no off-thread
        // path: it ran get_range_by_height + zstd/MessagePack decode of whole
        // bodies INLINE, so the expensive throttle above had to drop most of a
        // syncing peer's requests to protect production — measured live as a
        // producer-mode sigil-top burning 6 retry attempts per range
        // ("decode BackfillResp failed: unexpected end of file"). Now those
        // serves run on spawn_blocking readers with their OWN file handle
        // (serve_read::read_blocks_range), so the produce loop never pays for
        // them and they bypass the per-peer throttle entirely — bounded by this
        // counter instead: at most N disk readers at once, requests beyond the
        // cap are dropped exactly like the throttle drops them (caller retries).
        let serve_full_inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let serve_full_inflight_cap: usize = std::env::var("SIGIL_SERVE_FULL_INFLIGHT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(3);
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
                            // 2026-08-19 (deep-catchup stall investigation): every failure
                            // branch here used to be silent (`unwrap_or_default()` /
                            // `Err(_) => Vec::new()`), so a genuinely stuck request-ahead
                            // slot (send_request timeout/error, a malformed response, or an
                            // honestly-empty response body) was indistinguishable from "no
                            // work yet" — this is exactly what made a real, reproducible
                            // permanent sync stall look silent for hours. Log every branch
                            // that isn't a clean non-empty success.
                            let blocks = match serde_json::to_vec(&req) {
                                Ok(payload) => match mgr2.send_request(peer, payload).await {
                                    Ok(bytes) => match bincode::deserialize::<BackfillResp>(&bytes) {
                                        Ok(r) => {
                                            if r.blocks.is_empty() {
                                                eprintln!("⚠ rr-backfill(windowed): peer {peer} returned an EMPTY response for [{from}..={to}) — request-ahead slot stuck");
                                            }
                                            r.blocks
                                        }
                                        Err(e) => {
                                            eprintln!("⚠ rr-backfill(windowed): decode failed for [{from}..={to}) from {peer}: {e}");
                                            Vec::new()
                                        }
                                    },
                                    Err(e) => {
                                        eprintln!("⚠ rr-backfill(windowed): send_request failed for [{from}..={to}) to {peer}: {e}");
                                        Vec::new()
                                    }
                                },
                                Err(e) => {
                                    eprintln!("⚠ rr-backfill(windowed): request serialize failed: {e}");
                                    Vec::new()
                                }
                            };
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
                    //
                    // 2026-08-26 REVERTED (production stall, this session): this call site
                    // ran `dag_build_frontier_memo` for a few hours today; it stalled the
                    // live producer solid within ~2 minutes of every restart (0 blocks
                    // minted, high sustained CPU, mining/challenge 503ing) — the exact
                    // failure shape the 2026-08-23 attempt at this same optimization
                    // already produced once (see frontier.rs's module doc). The chronos
                    // soak that cleared it for adoption evidently didn't cover whatever
                    // broke tonight. Back on the plain, O(window) `dag_build_frontier` —
                    // slower (re-walks + re-applies the pending spine from the settled tip
                    // every tick, ~final_depth=512 re-applies/tick steady state) but proven
                    // stable; `frontier_cache` is no longer read, left declared for the next
                    // deliberate, chronos-first re-adoption attempt.
                    let _t_frontier = std::time::Instant::now(); // cheap; only READ when profiling
                    let _ = &frontier_cache;
                    let frontier_opt: Option<ChainTip> = braid.as_ref().map(|br| {
                        dag_build_frontier(&chain, br, &dag_bodies).frontier
                    });
                    ph_frontier_us += _t_frontier.elapsed().as_micros() as u64;
                    ph_ticks += 1;
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
                    // Gated production: mint only when a verified solve is waiting.
                    // An EXACT match (this frontier) embeds real PoW into the header,
                    // same as always. A NEAR-MISS (already verified by
                    // MiningBridge::submit() against the historical challenge it was
                    // actually solved for, within its credit window) still credits
                    // the real miner's wallet — but its nonce/blake4_hash/vdf are NOT
                    // embedded, because they were computed against a DIFFERENT parent
                    // than the one this block actually carries; embedding them would
                    // make the header's claimed PoW fail re-verification for every
                    // follower. Zeroed exactly like the "no solve" path — this block's
                    // PoW fields end up identical either way, only WHO gets paid
                    // changes. See sigil_api::mining::credit_window() for why this
                    // widening exists: measured live, 93.8% of supply had gone to the
                    // producer-wallet fallback because almost no real submission could
                    // win the EXACT-height race at this braid's block cadence.
                    // v7.1.41 (grogu-sync-perf, 2026-08-19, operator-directed — "all mining
                    // rewards should go to miners"): take_solve() pops exactly ONE FIFO
                    // entry. Under real multi-miner load, valid solves arrive faster than
                    // one-per-tick can examine them, so a single stale pop discarded the
                    // WHOLE tick's chance to credit anyone — fresher solves sitting right
                    // behind it in the queue just kept aging while they waited their turn,
                    // and by the time their turn came they'd often gone stale too. Measured
                    // live, this session: a fresh wallet mined 2 dual-lane-verified,
                    // API-"ACCEPTED" solves from a real remote miner; `queued_solves` grew
                    // steadily (5→7→8+) over the following minute while total network
                    // supply kept climbing (proving other mints WERE happening) and the
                    // wallet's own balance stayed at exactly 0 the whole time — the classic
                    // symptom this exact comment thread already diagnosed once before (see
                    // `credit_window`'s doc: "93.8% of supply had gone to the producer-
                    // wallet fallback"). Widening `credit_window` alone doesn't fix this
                    // half of the problem: with arrival-rate > one-per-tick drain-rate and a
                    // strict FIFO, sufncient backlog age-out happens regardless of how wide
                    // the window is. Fix: scan up to SOLVE_SCAN_MAX entries in ONE tick
                    // instead of giving up after the first stale pop — lets a tick catch up
                    // through a backlog instead of decaying it one entry at a time. Bounded
                    // (not unbounded) so a pathological backlog still can't stall block
                    // production indefinitely; anything scanned past and found stale is
                    // discarded exactly as before (no requeue — same accepted tradeoff,
                    // just now applied to up to SOLVE_SCAN_MAX candidates instead of 1).
                    let solve: Option<sigil_api::mining::AcceptedSolve> = if mining_gated {
                        match take_creditable_solve(&mining_bridge, mint_ref.parent_hash(), mint_ref.height()) {
                            Some(s) => Some(s),
                            None => continue, // no creditable work this tick — the braid idles, as PoW should
                        }
                    } else {
                        // Free-running (unchanged cadence, unchanged GHOSTDAG throughput):
                        // opportunistically credit a solve that's ALREADY queued —
                        // exact match or a near-miss within the credit window — but
                        // never wait for one. Anything that doesn't qualify after scanning
                        // falls through to the producer-wallet default.
                        take_creditable_solve(&mining_bridge, mint_ref.parent_hash(), mint_ref.height())
                    };
                    // pull verify-once txs (already verified at mempool ingest),
                    // plus every wallet-authenticated send queued since the last
                    // tick (verified in `SendBridge::submit`, not here — see its
                    // docs). Unlike the mempool pull, this drain is unconditional:
                    // a real send must never depend on the SIGIL_TXGEN load-gen
                    // flag being set.
                    // v7.1.29: SNAPSHOT, not drain. This braid mints several competing
                    // candidate blocks at the SAME height before settlement picks a
                    // winner (measured live: 3 candidates at h=1109490 racing off the
                    // same parent) — a destructive drain hands a pending send to
                    // whichever candidate happens to be minting at that instant, and
                    // if THAT candidate is the one that gets orphaned, the send is
                    // gone forever with no retry, even though it was never rejected.
                    // `snapshot_for_mint` instead re-embeds every still-pending send
                    // into EVERY candidate until `SendBridge::confirm_applied` (called
                    // only once a candidate is confirmed on the SETTLED spine, see
                    // below) retires it — so it doesn't matter which candidate wins,
                    // the send rides along on all of them until one actually lands.
                    let block_txs: Vec<SignedTx> = {
                        let mut v: Vec<SignedTx> =
                            if txgen > 0 { mempool.pull(txgen) } else { Vec::new() };
                        v.extend(send_bridge.snapshot_for_mint());
                        v.extend(nation_bridge.snapshot_for_mint());
                        v.extend(shielded_bridge.snapshot_for_mint());
                        v.extend(bridge_bridge.snapshot_for_mint());
                        v.extend(dex_bridge.snapshot_for_mint());
                        v.extend(usds_bridge.snapshot_for_mint());
                        v.extend(usds_polygon_bridge.snapshot_for_mint());
                        v
                    };
                    // ONE-CHAIN: when the adaptive emission controller is live, IT
                    // computes the reward (time-based + PID + rate) and we bake the
                    // exact amount into the coinbase; else the pure height schedule.
                    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let reward_override: Option<u128> = emission.as_mut().map(|c| {
                        let supply = mint_ref.state_snapshot().native_supply();
                        c.calculate_block_reward(now_secs, supply)
                    });
                    let topology_commitment = compute_topology_commitment(braid.as_ref(), mint_ref.height());
                    // Drain the partial-share pool ONLY for a self-minted block.
                    // With a real solve the winner's own `shares` map already
                    // carries the pool (submit() folds and clears it), so draining
                    // here too would pay the same work twice.
                    let share_pool = if solve.is_none() { mining_bridge.take_share_pool() } else { None };
                    // 2026-08-26 (rocky-lead) — decide the ATTRIBUTION before the pool
                    // is moved into the mint. This mirrors mint.rs's own choice; it
                    // does not make it. Keeping it here rather than threading a return
                    // value back out of mint_next_block leaves that file (another
                    // agent's live Option C work) untouched.
                    //
                    // Why this is worth a few lines: the "producer wallet takes ~94%
                    // while real miners take ~0%" failure has now happened TWICE and
                    // was invisible both times — shares accepted, blocks produced,
                    // supply climbing, hashrate up, every metric green. Finding it took
                    // a bespoke off-box experiment. See sigil_api::attribution.
                    let pay_pool = std::env::var("SIGIL_PAY_SHARE_POOL")
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false);
                    let (payout_source, weights): (
                        sigil_api::attribution::PayoutSource,
                        Option<&std::collections::HashMap<WalletId, u64>>,
                    ) = if let Some(s) = solve.as_ref() {
                        (sigil_api::attribution::PayoutSource::RealSolve, Some(&s.shares))
                    } else if pay_pool && share_pool.as_ref().is_some_and(|p| !p.is_empty()) {
                        (sigil_api::attribution::PayoutSource::SharePool, share_pool.as_ref())
                    } else {
                        (sigil_api::attribution::PayoutSource::ProducerFallback, None)
                    };
                    // 2026-08-26 (rocky) — how much of this block's miner slice lands as a
                    // PRIVATE note rather than a transparent balance.
                    //
                    // Without this the ledger lies by omission, and it very nearly did:
                    // a miner holding 33.1% of network hashrate showed a transparent-balance
                    // delta of exactly 0 over 150 s, which reads as "unpaid" and was about to
                    // be reported as a broken payout split. It was registered for shielded
                    // rewards — the money went into the pool as notes (2 → 2,890 notes) and
                    // `shielded_hps_pct` matched its hashrate almost exactly. An attribution
                    // ledger that can only see transparent balances reproduces the exact
                    // blind spot it exists to remove.
                    //
                    // Weight share is the right proxy for value share because the coinbase
                    // splits proportionally to these same weights. Costs one registry lookup
                    // per payee, and payees average <2.
                    let (payees, shielded_payees, shielded_pct) = match weights {
                        None => (1u32, 0u32, 0.0f32),
                        Some(w) if w.is_empty() => (1u32, 0u32, 0.0f32),
                        Some(w) => {
                            let snap = mint_ref.state_snapshot();
                            let pool = snap.shielded();
                            let total: u128 = w.values().map(|v| *v as u128).sum();
                            let mut sh_n = 0u32;
                            let mut sh_w: u128 = 0;
                            for (wallet, weight) in w.iter() {
                                if pool.shielded_address(wallet).is_some() {
                                    sh_n += 1;
                                    sh_w += *weight as u128;
                                }
                            }
                            let pct = if total == 0 { 0.0 } else { (sh_w as f64 * 100.0 / total as f64) as f32 };
                            (w.len().max(1) as u32, sh_n, pct)
                        }
                    };
                    match mint_next_block(mint_ref, mp, &block_txs, reward_override, solve.as_ref(), topology_commitment, share_pool) {
                        Ok((block, minted_tx_hashes)) => {
                            let h = block.header.height;
                            sigil_api::attribution::record(
                                h,
                                payout_source,
                                payees,
                                reward_override.unwrap_or(0),
                                shielded_payees,
                                shielded_pct,
                            );
                            // Fire ONLY on a real, sustained fairness failure. An alarm
                            // that shouts during normal operation is worse than no
                            // alarm — it trains everyone to scroll past the real one.
                            if h % 512 == 0 {
                                let att = sigil_api::attribution::summary(0);
                                if att.alarm {
                                    eprintln!("{}", sigil_api::attribution::verdict(&att));
                                }
                            }
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
                            let bytes = chain_log::encode_record(&block).unwrap_or_default();
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
                                    // Remember what THIS specific candidate carries, keyed by
                                    // its own hash — dag_drain_apply looks this up ONLY for the
                                    // candidate(s) that actually land on the settled spine, so
                                    // an orphaned sibling's entry just goes stale (pruned below)
                                    // and its sends stay pending for the next mint attempt.
                                    if !minted_tx_hashes.is_empty() {
                                        mint_hash_to_tx_hashes.insert(vh, minted_tx_hashes);
                                    }
                                    if mint_hash_to_tx_hashes.len() > MINT_HASH_TRACKING_CAP {
                                        prune_mint_hash_tracking(&mut mint_hash_to_tx_hashes, &dag_bodies);
                                    }
                                }
                                // the ONE settlement path — identical finalized order on every node
                                let _t_drain = std::time::Instant::now();
                                let (a, s, f) = dag_drain_apply(br, &mut dag_bodies, &mut chain,
                                    &mut |braw| { let _ = chain_log.append_bytes(braw); },
                                    &send_bridge, &bridge_bridge, &dex_bridge, &usds_bridge, &usds_polygon_bridge,
                                    &shielded_bridge, &mut mint_hash_to_tx_hashes);
                                applied += a; dag_ord_skipped += s; dag_apply_failed += f;
                                ph_drain_us += _t_drain.elapsed().as_micros() as u64;
                                true
                            } else {
                                match chain.apply(block) {
                                    Ok(_) => {
                                        let _ = chain_log.append_bytes(&bytes);
                                        // Linear mode applies its own block synchronously, right
                                        // here — no candidate-racing, so no lookup needed: these
                                        // ARE the hashes that just landed.
                                        if !minted_tx_hashes.is_empty() {
                                            send_bridge.confirm_applied(&minted_tx_hashes);
                                            nation_bridge.confirm_applied(&minted_tx_hashes);
                                            shielded_bridge.confirm_applied(&minted_tx_hashes);
                                            bridge_bridge.confirm_applied(&minted_tx_hashes);
                                            dex_bridge.confirm_applied(&minted_tx_hashes);
                                            usds_bridge.confirm_applied(&minted_tx_hashes);
                                            usds_polygon_bridge.confirm_applied(&minted_tx_hashes);
                                        }
                                        true
                                    }
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
                                            let backlog = mempool.len();
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
                                    // Phase 2 finality vote. Discarding this value
                                    // has no effect on the chain — see the safety
                                    // posture in finality_wire.rs.
                                    if let Some(vb) = finality.on_block(
                                        h, bhash, topology_commitment, finality_wire::now_ms()
                                    ) {
                                        if let Err(e) = mgr.publish(sigil_net::TOPIC_FINALITY_VOTES, vb) {
                                            eprintln!("⚠ publish finality vote H={} failed: {}", h, e);
                                        }
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
                _ = dag_snapshot_tick.tick() => {
                    if let Some(br) = braid.as_ref() {
                        dag_snapshot_bridge.update(br.recent_summary(200), br.ghostdag_k());
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
                        // Phase 2's whole deliverable: the measured comparison
                        // between a certificate and today's 512-block depth rule.
                        // Silent unless finality is configured.
                        if let Some(line) = finality.heartbeat_line(chain.height()) {
                            eprintln!("🔗 {}", line);
                        }

                        // Fail-loud finality monitoring — see `last_below_final`'s
                        // doc comment above. `below_final` counts blocks the
                        // finality clamp has PERMANENTLY refused (real, already
                        // happened); `finality_margin` is the early-warning
                        // signal for blocks already pending that are close to
                        // suffering the same fate. Neither indicates active
                        // danger on today's real, single-producer chain (there
                        // is no second producer to reorder against), but this
                        // is exactly the signal that needs to be loud the day
                        // that stops being true.
                        if let Some(br) = braid.as_ref() {
                            let s = br.stats();
                            // 2026-08-26: alarm on blocks we ACTUALLY LOST, not on the
                            // guard doing its job. This previously watched `below_final`,
                            // which also counts stale blocks refused at the door — the
                            // routine consequence of serving backfill to a syncing peer,
                            // which re-offers history we long since finalized. Measured
                            // live on Epsilon that day: bursts of 60-70 every couple of
                            // minutes, every one of them announced as a PERMANENT ORPHAN
                            // and none of them a loss at all. An alarm that fires on
                            // normal traffic is worse than no alarm, because it trains
                            // everyone to scroll past the real one.
                            if s.below_final_dropped > last_below_final {
                                let newly_lost = s.below_final_dropped - last_below_final;
                                eprintln!(
                                    "🚨 FINALITY VIOLATION: {} block(s) the braid was holding have been PERMANENTLY \
                                     dropped (parent never arrived, or finality advanced past them) — \
                                     total dropped={} refused_at_door={} finalized_height={}",
                                    newly_lost, s.below_final_dropped, s.below_final_refused, s.finalized_height
                                );
                            }
                            last_below_final = s.below_final_dropped;
                            if let Some(margin) = s.finality_margin {
                                if margin == 0 {
                                    eprintln!(
                                        "🚨 finality margin EXHAUSTED — the next finality advance will orphan a pending block \
                                         (pending={} finalized_height={})",
                                        s.pending, s.finalized_height
                                    );
                                } else if margin < FINALITY_MARGIN_WARN_THRESHOLD {
                                    eprintln!(
                                        "⚠ finality margin low: {} heights of safety remain before a pending block is orphaned \
                                         (pending={})",
                                        margin, s.pending
                                    );
                                }
                            }
                        }
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
                            // Serving runs INLINE in this loop, so its cost is
                            // stolen directly from block production. Measured here
                            // so the breakdown below can prove or refute that,
                            // instead of it being argued from log rates.
                            let _serve_guard = if mint_profile {
                                Some(ServeTimer { start: std::time::Instant::now(), acc: &ph_serve_us })
                            } else {
                                None
                            };
                            // Point-to-point backfill serve: answer ONE requester with
                            // the requested block range straight from our chain. No
                            // gossipsub re-broadcast — the response goes only to `peer`.
                            //
                            // A Dandelion++ stem hop (dandelion_relay::StemWireMsg) rides
                            // this SAME point-to-point channel — never gossipsub, or the
                            // whole reason for stem phase (no broadcast until fluff) would
                            // be defeated. It's bincode, BackfillReq is JSON, so trying
                            // BackfillReq first and falling through on failure cleanly
                            // disambiguates the two without a wire-format tag byte.
                            let req: BackfillReq = match serde_json::from_slice(&payload) {
                                Ok(r) => r,
                                Err(_) => {
                                    if let Ok(stem) = bincode::deserialize::<crate::dandelion_relay::StemWireMsg>(&payload) {
                                        let _ = dandelion_tx.send(crate::dandelion_relay::Cmd::StemIncoming {
                                            id: stem.id,
                                            hops: stem.hops,
                                            bytes: stem.tx_bytes,
                                        });
                                        // Ack only — the actual stem/fluff decision happens
                                        // asynchronously in the Dandelion actor and the sender
                                        // doesn't need to know the outcome, only that this hop
                                        // received it (send_request needs SOME response).
                                        mgr.respond(request_id, vec![1u8]);
                                    } else if let Ok(hello) = bincode::deserialize::<crate::wg_relay::WgHelloMsg>(&payload) {
                                        // wg_relay's key-exchange (see its module docs): a peer
                                        // offering their WG identity, expecting ours back in
                                        // the SAME response (one round trip, bidirectional).
                                        if let Some(wg) = wg_state.as_ref() {
                                            crate::wg_relay::handle_hello(wg, peer, &hello);
                                            let ours = crate::wg_relay::our_hello(wg);
                                            if let Ok(payload) = bincode::serialize(&ours) {
                                                mgr.respond(request_id, payload);
                                            }
                                        }
                                    }
                                    continue;
                                }
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

                            // ── Drain off-thread serves that finished since the last request,
                            // so their bytes populate the caches checked immediately below.
                            // Non-blocking; an empty channel costs one atomic load. Deliberately
                            // here rather than as a select! arm — this loop drives block
                            // production and is not somewhere to add wakeup sources lightly,
                            // and requests arrive constantly so the fill is never far behind.
                            while let Ok((k, imm, hot, blob)) = serve_done_rx.try_recv() {
                                let sz = blob.len();
                                if imm {
                                    if let Some(prev) = serve_cache.insert(k, blob) {
                                        serve_cache_bytes = serve_cache_bytes.saturating_sub(prev.len());
                                    }
                                    serve_cache_order.push_back(k);
                                    serve_cache_bytes = serve_cache_bytes.saturating_add(sz);
                                    while serve_cache_bytes > serve_cache_budget {
                                        match serve_cache_order.pop_front() {
                                            Some(kk) => {
                                                if let Some(v) = serve_cache.remove(&kk) {
                                                    serve_cache_bytes = serve_cache_bytes.saturating_sub(v.len());
                                                }
                                            }
                                            None => break,
                                        }
                                    }
                                } else if hot {
                                    if let Some((prev, _)) = hot_cache.insert(k, (blob, std::time::Instant::now())) {
                                        hot_cache_bytes = hot_cache_bytes.saturating_sub(prev.len());
                                    }
                                    hot_cache_order.push_back(k);
                                    hot_cache_bytes = hot_cache_bytes.saturating_add(sz);
                                    while hot_cache_bytes > hot_cache_budget {
                                        match hot_cache_order.pop_front() {
                                            Some(kk) => {
                                                if let Some((v, _)) = hot_cache.remove(&kk) {
                                                    hot_cache_bytes = hot_cache_bytes.saturating_sub(v.len());
                                                }
                                            }
                                            None => break,
                                        }
                                    }
                                }
                            }

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
                            // ── HOT CACHE HIT: same idea, short-TTL, covers near-tip ranges
                            // the long-lived cache above deliberately excludes. codec>=3 ('P'
                            // snapshot header / 'F' fold trailer) stays excluded — those encode
                            // the CURRENT anchor/tip, not just [lo,hi], so a stale copy would
                            // lie about the anchor. Everything else in [lo,hi] is already
                            // durably committed on this node (hi is clamped to chain.height()-1
                            // above) so a short-lived exact-range replay is always correct, not
                            // just fast.
                            let hot_eligible = !immutable && hi >= lo && req.codec < 3;
                            if hot_eligible {
                                if let Some((blob, at)) = hot_cache.get(&ckey) {
                                    if at.elapsed() < hot_cache_ttl {
                                        hot_hits += 1;
                                        mgr.respond(request_id, blob.as_ref().clone());
                                        continue;
                                    }
                                }
                                hot_miss += 1;
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

                            // 2026-08-22 (root cause of the multi-hour production stall, found
                            // live): a headers-only serve was assumed cheap enough to never need
                            // throttling — true for a genuinely cached range, but this point in
                            // the code is reached ONLY on a cache MISS from both caches above, and
                            // a miss here means a real disk read (`get_range_by_height`) plus
                            // serialize plus zstd compress of up to `serve_cap` headers (~32k),
                            // running INLINE in the same synchronous loop that drives block
                            // production. Proven live: a single peer retrying an uncacheable
                            // near-tip range ~2×/s alone stalled the 5s heartbeat out to 43-69s.
                            // The hot cache above cuts most of that traffic, but under sustained
                            // pressure from one or more aggressive/retrying peers, misses still
                            // pile up faster than they can safely be serviced inline. Cap the
                            // worst case directly, the same way the (now-vestigial) full-block
                            // throttle above was originally meant to: at most one expensive
                            // compute per `throttle` interval, GLOBALLY, across every peer and
                            // every codec. A request that arrives inside the window is dropped,
                            // not queued or blocked — safe, because every caller here already
                            // retries on its own cadence (documented at the top of this arm), so
                            // dropping is a bounded delay, never data loss. This is a hard ceiling
                            // on how much wall-clock time backfill-serving can ever take from
                            // block production, independent of how many peers are asking or how
                            // often they retry.
                            let expensive_throttle = std::time::Duration::from_millis(
                                std::env::var("SIGIL_SERVE_EXPENSIVE_THROTTLE_MS").ok()
                                    .and_then(|v| v.parse::<u64>().ok()).unwrap_or(120),
                            );
                            // 2026-08-23 — ROOT CAUSE OF "NODES DON'T SYNC ANY MORE", found
                            // live on the production producer: the throttle above was GLOBAL
                            // (one slot per interval shared by every peer). Measured on
                            // Epsilon: ONE peer retrying the same uncacheable near-tip range
                            // accounted for 102 of 116 requests in 3 minutes and consumed the
                            // budget, so a genuinely-syncing archive node got ~1 served range
                            // per minute and its remaining requests were DROPPED (159
                            // expensive-throttled in one window) — reported by the client as
                            // "STALLED, rate 0 blk/s, fetched 0" against a 1.77M-block gap.
                            // The mitigation was punishing the innocent: a runaway peer and a
                            // healthy syncing node competed for the same single slot, and the
                            // runaway (asking ~2x/s) won essentially every time.
                            //
                            // Fix: throttle PER PEER. A peer that hammers now throttles only
                            // ITSELF, at exactly the same rate as before, while every other
                            // peer keeps its own independent budget. The protection this
                            // throttle exists for is preserved — the guarded work is still
                            // bounded per peer per interval — but it is no longer a
                            // single point of contention that one bad actor can monopolize.
                            // A global backstop remains below so N peers can't sum to an
                            // unbounded inline cost.
                            // ── OFF-THREAD FULL-BLOCK SERVE (2026-08-31) ────────────────
                            //
                            // The deep-history full-block request — exactly what a
                            // producer-mode sigil-top sends while catching up — was the
                            // last serve class still running INLINE (decompress +
                            // MessagePack-decode of every body on the produce loop), which
                            // is why the expensive throttle had to drop most of them and a
                            // syncing client saw "decode BackfillResp failed: unexpected
                            // end of file" six retries per range. Serve it the same way the
                            // header path has been served off-thread since 2026-08-26: a
                            // spawn_blocking reader with its OWN file handle
                            // (serve_read::read_blocks_range — never chain_log's append
                            // handle), the RAM-window tail cloned here while we hold
                            // `chain`, byte-identical BackfillResp encoding, respond-first,
                            // then cache-fill through the same done channel. These serves
                            // BYPASS the per-peer expensive throttle — the produce loop no
                            // longer pays for them — and are bounded by the in-flight cap
                            // declared with `serve_full_inflight` instead; at the cap the
                            // request is dropped exactly as the throttle would (the client
                            // already retries on its own cadence).
                            if !req.headers_only && req.codec < 3 && lo < wbase {
                                use std::sync::atomic::Ordering;
                                if serve_full_inflight.load(Ordering::Relaxed) >= serve_full_inflight_cap {
                                    expensive_throttled += 1;
                                    continue;
                                }
                                serve_full_inflight.fetch_add(1, Ordering::Relaxed);
                                let window_blocks: Vec<crate::block::Block> =
                                    (lo.max(wbase)..=hi).filter_map(|h| chain.get(h).cloned()).collect();
                                let disk_hi = hi.min(wbase.saturating_sub(1));
                                let dir = snap_dir.clone();
                                let mgr2 = std::sync::Arc::clone(&mgr);
                                let done = serve_done_tx.clone();
                                let inflight = std::sync::Arc::clone(&serve_full_inflight);
                                let peer2 = peer;
                                tokio::task::spawn_blocking(move || {
                                    let mut blocks = crate::serve_read::read_blocks_range(&dir, lo, disk_hi);
                                    blocks.extend(window_blocks);
                                    let n = blocks.len();
                                    let resp = BackfillResp { blocks };
                                    let blob = std::sync::Arc::new(
                                        bincode::serialize(&resp).unwrap_or_default(),
                                    );
                                    // Respond FIRST — the peer must not wait on our caching.
                                    mgr2.respond(request_id, blob.as_ref().clone());
                                    eprintln!("↩ rr-backfill(off-thread): served {} BLOCKS [{}..={}] to {} ({} B)",
                                        n, lo, hi, peer2, blob.len());
                                    let _ = done.try_send((ckey, immutable, hot_eligible, blob));
                                    inflight.fetch_sub(1, Ordering::Relaxed);
                                });
                                continue;
                            }

                            let peer_key = peer.to_string();
                            if last_expensive_by_peer
                                .get(&peer_key)
                                .is_some_and(|t| t.elapsed() < expensive_throttle)
                            {
                                expensive_throttled += 1;
                                continue;
                            }
                            // Global backstop: far looser than the per-peer interval, so many
                            // honest peers are all served promptly, but the total inline cost
                            // per unit time still has a hard ceiling.
                            let global_floor = expensive_throttle / 8;
                            if last_expensive_serve.elapsed() < global_floor {
                                expensive_throttled += 1;
                                continue;
                            }
                            let now_inst = std::time::Instant::now();
                            last_expensive_serve = now_inst;
                            last_expensive_by_peer.insert(peer_key, now_inst);
                            // Keep the per-peer map bounded regardless of peer churn.
                            if last_expensive_by_peer.len() > 512 {
                                last_expensive_by_peer
                                    .retain(|_, t| t.elapsed() < std::time::Duration::from_secs(300));
                            }

                            // ── OFF-THREAD SERVE (2026-08-26, rocky-lead) — THE actual fix ──
                            //
                            // Measured on the REAL chain log (serve_read's
                            // `bench_header_only_vs_full_block_on_the_real_log`): decoding one
                            // 8,193-record serve range costs SECONDS, and header-only decoding
                            // is only ~1.18× cheaper than full-block decoding — serde_json still
                            // tokenises every byte of every record even when it throws the
                            // fields away. So making the decode cheaper was never going to be
                            // enough on its own. The work has to leave the loop that produces
                            // blocks, which is what this does.
                            //
                            // Offloaded exactly when the request needs a DISK read
                            // (`lo < wbase`) and is headers-only with a range-local codec —
                            // those are the requests whose every input is owned and `Send`: a
                            // directory path, the range, and the in-RAM window headers (copied
                            // here, cheaply, while we still hold `chain`).
                            //
                            // Deliberately NOT offloaded:
                            //   · codec 3 ('P') / 4 ('F') — they encode the CURRENT anchor/tip,
                            //     not just [lo,hi], so they must read live chain state;
                            //   · full-block serves — they need `chain_log`, which owns an
                            //     append handle and is not shareable;
                            //   · anything already served from cache above (free, stays inline).
                            //
                            // Spawn rate is bounded by the per-peer expensive throttle right
                            // above, so this cannot become a task storm.
                            if req.headers_only && req.codec < 3 && lo < wbase {
                                let window_headers: Vec<sigil_header::SigilBlockHeaderV0> =
                                    (lo.max(wbase)..=hi)
                                        .filter_map(|h| chain.get(h).map(|b| b.header.clone()))
                                        .collect();
                                let disk_hi = hi.min(wbase.saturating_sub(1));
                                let dir = snap_dir.clone();
                                let mgr2 = std::sync::Arc::clone(&mgr);
                                let done = serve_done_tx.clone();
                                let codec = req.codec;
                                let peer2 = peer;
                                tokio::task::spawn_blocking(move || {
                                    let mut hs = crate::serve_read::read_headers_range(&dir, lo, disk_hi);
                                    hs.extend(window_headers);
                                    let blob = std::sync::Arc::new(
                                        crate::serve_read::encode_headers(&hs, codec),
                                    );
                                    // Respond FIRST — the peer must not wait on our caching.
                                    mgr2.respond(request_id, blob.as_ref().clone());
                                    eprintln!("↩ rr-backfill(off-thread): served {} HEADERS [{}..={}] to {} ({} B, codec {})",
                                        hs.len(), lo, hi, peer2, blob.len(), codec);
                                    // Hand the bytes back for caching. try_send, never send: a
                                    // full channel drops a cache FILL, never a response.
                                    let _ = done.try_send((ckey, immutable, hot_eligible, blob));
                                });
                                continue;
                            }

                            // Gather the range: disk portion via ONE sequential read,
                            // recent portion from the in-RAM window.
                            //
                            // 2026-08-26 (serve-path LIVENESS fix — rocky-lead): a
                            // headers_only request now takes a HEADER-ONLY disk read and
                            // never materialises a block body. Every headers_only codec
                            // (0/1 'H'/'Z', 2 'S' skeletons, 4 'F' trailer) consumes only
                            // `b.header`, yet this call site used to decode whole blocks —
                            // bodies, transactions, state mutations — and drop them. A
                            // header is ~70 B, so serving 8,193 of them (≈573 KB) was
                            // JSON-decoding megabytes of bodies to produce it, INLINE on
                            // the loop that also produces blocks.
                            //
                            // That is not a throughput nicety. On 2026-08-26 it stalled the
                            // producer outright: three independent perf profiles of the live
                            // node (three agents, three sample windows) all converged on
                            // get_range_by_height + serde_json parse_number/parse_integer +
                            // malloc/_int_free. The 27 ms produce tick never got the loop
                            // back, publish_tip() never ran, /v1/mining/challenge answered
                            // 503, and five live miners at ~54 MH/s earned nothing while the
                            // chain sat frozen. See serve_read.rs's module doc.
                            //
                            // Full-block serves (headers_only == false) are UNCHANGED and
                            // still go through get_range_by_height — including its
                            // 2026-08-21 fix (NOT the raw offsets-indexed get_range, which
                            // silently returns zero blocks once a node's offsets array has
                            // drifted from real height; confirmed live answering a real
                            // peer with 0 headers for a range the node had on disk).
                            let mut blocks: Vec<crate::block::Block> = Vec::new();
                            let mut headers: Vec<sigil_header::SigilBlockHeaderV0> = Vec::new();
                            if req.headers_only {
                                if lo < wbase {
                                    let disk_hi = hi.min(wbase.saturating_sub(1));
                                    headers.extend(crate::serve_read::read_headers_range(&snap_dir, lo, disk_hi));
                                }
                                for h in lo.max(wbase)..=hi {
                                    if let Some(b) = chain.get(h) { headers.push(b.header.clone()); }
                                }
                            } else {
                                if lo < wbase {
                                    let disk_hi = hi.min(wbase.saturating_sub(1));
                                    blocks.extend(chain_log.get_range_by_height(lo, disk_hi));
                                }
                                for h in lo.max(wbase)..=hi {
                                    if let Some(b) = chain.get(h) { blocks.push(b.clone()); }
                                }
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
                                    // ONE encoder, shared with the off-thread serve path
                                    // below — see serve_read::encode_headers.
                                    let o = crate::serve_read::encode_headers(&headers, 2);
                                    eprintln!("↩ rr-backfill: served {} SKELETONS 'S' [{}..={}] to {} ({} B)",
                                        headers.len(), lo, hi, peer, o.len());
                                    o
                                } else if req.codec == 4 {
                                    // M1 fold-trailer: archive_root = BLAKE3 over bincode of each SkeletonRecord in order
                                    // (matches the client SnapshotVerifier::push). anchor_sig/fold_blob empty = the
                                    // structural pull the client finalize accepts on root-match; M2 adds SQIsign+flux_fold.
                                    // 2026-08-26 (rocky-lead) — this arm was the single most
                                    // dangerous thing in the serve path. `archive_root` is a
                                    // BLAKE3 over EVERY SkeletonRecord in the chain, and it was
                                    // computed by reading and JSON-decoding every block from
                                    // height 0 to window_base — ~2.18 M FULL blocks, bodies and
                                    // all, collected into one `Vec<Block>` in RAM — inline on
                                    // the produce loop, with no cache, on EVERY request. One
                                    // peer sending codec=4 could stall block production for
                                    // minutes and add gigabytes of RSS. Three changes, none of
                                    // which alter the computed root:
                                    //   1. header-only read — the root only ever consumed
                                    //      headers, so no body is decoded any more;
                                    //   2. streamed in chunks — bounded RAM instead of holding
                                    //      the whole chain at once;
                                    //   3. memoised on the anchor height — repeated requests at
                                    //      the same tip are a compare, not a rescan.
                                    // ⚠️ HONEST LIMIT: this is still O(chain) on the first
                                    // request at each new anchor, and the anchor advances every
                                    // block. It is a large constant-factor improvement, NOT a
                                    // fix. A rolling/incremental archive root is an open lane.
                                    const FOLD_CHUNK: u64 = 8192;
                                    let ga = chain.height().saturating_sub(1);
                                    let archive_root = match fold_root_memo {
                                        Some((h, r)) if h == ga => r,
                                        _ => {
                                            let gw = chain.window_base();
                                            let mut hh = blake3::Hasher::new();
                                            let mut at = 0u64;
                                            while at < gw {
                                                let end = at.saturating_add(FOLD_CHUNK - 1).min(gw.saturating_sub(1));
                                                for hd in crate::serve_read::read_headers_range(&snap_dir, at, end) {
                                                    hh.update(&bincode::serialize(&SkeletonRecord::from_header(&hd)).unwrap_or_default());
                                                }
                                                at = end.saturating_add(1);
                                            }
                                            for h2 in gw..=ga {
                                                if let Some(b2) = chain.get(h2) {
                                                    hh.update(&bincode::serialize(&SkeletonRecord::from_header(&b2.header)).unwrap_or_default());
                                                }
                                            }
                                            let r = *hh.finalize().as_bytes();
                                            fold_root_memo = Some((ga, r));
                                            r
                                        }
                                    };
                                    let trailer = sigil_header::SnapshotTrailer { archive_root, anchor_sig: Vec::new(), fold_blob: Vec::new() };
                                    let mut o = vec![b'F'];
                                    o.extend(bincode::serialize(&trailer).unwrap_or_default());
                                    eprintln!("served 'F' trailer M1 (root) [{}..={}] to {} ({} B)", lo, hi, peer, o.len());
                                    o
                                } else {
                                    // codec 0/1: monitor path — bincode Vec<header>, ~20× smaller,
                                    // no JSON. `headers` came straight off the header-only read
                                    // above — no block body was ever decoded to build it. ONE
                                    // encoder, shared with the off-thread serve path below.
                                    let out = crate::serve_read::encode_headers(&headers, req.codec);
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
                                // Evict by BYTES, not by entry count — the entries are
                                // 0.3-3.3 MB each, so a count cap bounds nothing.
                                let sz = out.len();
                                if let Some(prev) = serve_cache.insert(ckey, std::sync::Arc::new(out.clone())) {
                                    serve_cache_bytes = serve_cache_bytes.saturating_sub(prev.len());
                                }
                                serve_cache_order.push_back(ckey);
                                serve_cache_bytes = serve_cache_bytes.saturating_add(sz);
                                while serve_cache_bytes > serve_cache_budget {
                                    match serve_cache_order.pop_front() {
                                        Some(k) => {
                                            if let Some(v) = serve_cache.remove(&k) {
                                                serve_cache_bytes = serve_cache_bytes.saturating_sub(v.len());
                                            }
                                        }
                                        None => break, // nothing left to evict
                                    }
                                }
                            }
                            // ── HOT CACHE FILL (near-tip ranges), short-TTL + FIFO-capped ──
                            if hot_eligible && !out.is_empty() {
                                let sz = out.len();
                                if let Some((prev, _)) = hot_cache.insert(ckey, (std::sync::Arc::new(out.clone()), std::time::Instant::now())) {
                                    hot_cache_bytes = hot_cache_bytes.saturating_sub(prev.len());
                                }
                                hot_cache_order.push_back(ckey);
                                hot_cache_bytes = hot_cache_bytes.saturating_add(sz);
                                while hot_cache_bytes > hot_cache_budget {
                                    match hot_cache_order.pop_front() {
                                        Some(k) => {
                                            if let Some((v, _)) = hot_cache.remove(&k) {
                                                hot_cache_bytes = hot_cache_bytes.saturating_sub(v.len());
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                if last_hot_cache_log.elapsed() >= std::time::Duration::from_secs(5) {
                                    let hot_tot = (hot_hits + hot_miss).max(1);
                                    if mint_profile && last_phase_log.elapsed() >= std::time::Duration::from_secs(10) {
                                        let secs = last_phase_log.elapsed().as_secs_f64().max(0.001);
                                        let t = ph_ticks.max(1);
                                        let serve_us_now = ph_serve_us.load(std::sync::atomic::Ordering::Relaxed);
                                        eprintln!("⏱ mint-tick self-profile (per tick avg over {} ticks / {:.0}s): frontier {:.1}ms · settle-drain {:.1}ms · inline-serve {:.1}ms — serve is {:.0}% of this loop's measured time",
                                            ph_ticks, secs,
                                            ph_frontier_us as f64 / t as f64 / 1000.0,
                                            ph_drain_us as f64 / t as f64 / 1000.0,
                                            serve_us_now as f64 / t as f64 / 1000.0,
                                            serve_us_now as f64 * 100.0 / (ph_frontier_us + ph_drain_us + serve_us_now).max(1) as f64);
                                        ph_frontier_us = 0; ph_drain_us = 0; ph_ticks = 0;
                                        ph_serve_us.store(0, std::sync::atomic::Ordering::Relaxed);
                                        last_phase_log = std::time::Instant::now();
                                    }
                                    eprintln!("⚡ hot-cache: {} hits / {} miss ({:.0}% hit) · {} chunks RAM · ttl={}ms · {} expensive-throttled",
                                        hot_hits, hot_miss, hot_hits as f64 * 100.0 / hot_tot as f64, hot_cache.len(), hot_cache_ttl.as_millis(), expensive_throttled);
                                    last_hot_cache_log = std::time::Instant::now();
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
                                // WIRE v1 (2026-08-27): blocks are gossiped in the same
                                // versioned record encoding the chain log uses — msgpack+zstd,
                                // measured 5.13x smaller than the JSON this used to carry, on
                                // what is by far the highest-volume topic on the network.
                                //
                                // `decode_record` accepts BOTH the new form and legacy JSON, so
                                // this stays readable from any peer that has not updated yet.
                                // The tolerance is the whole compatibility story: publish new,
                                // accept old.
                                let block: crate::block::Block = match chain_log::decode_record(&data) {
                                    Some(b) => b,
                                    None => continue,
                                };
                                received += 1;
                                if let Some(br) = braid.as_mut() {
                                    // SIGIL_DAG=1 REAL path (design §3.2): precheck →
                                    // braid.insert → park+backfill on missing parents →
                                    // drain the finalized order and state-apply exactly
                                    // the spine blocks that extend the local tip through
                                    // the UNMODIFIED ChainTip::apply chokepoint.
                                    tx_total += block.header.tx_count as u64;
                                    // 2026-08-20: verify_at_height — real Ed25519Hot
                                    // signature check once activated, no-op otherwise.
                                    // Early/cheap reject only (ChainTip::apply below
                                    // remains the authoritative chokepoint either way) —
                                    // just avoids wasting braid/DAG bookkeeping on a
                                    // block that would be rejected downstream anyway.
                                    if let Err(e) = block.header.verify_at_height(block.header.height) {
                                        eprintln!("⚠ braid: precheck reject from {}: {}", from, e);
                                        continue;
                                    }
                                    let bhash = block.hash();
                                    let bheight = block.header.height;
                                    let block_producer = block.header.producer;
                                    if bheight > net_tip { net_tip = bheight; }
                                    // QTFT Path C (SIGIL-level v1): note which peer relayed which
                                    // producer's strand, so a later gap for THAT producer can
                                    // prefer a peer topologically close to it (has actually been
                                    // carrying that strand) over an arbitrary connected peer.
                                    peer_affinity.record(from.clone(), block_producer, bheight);
                                    // QTFT-2: verify BEFORE insert — the window this block's
                                    // topology_commitment covers is `[height-32, height-1]`,
                                    // strictly ancestors, so it never includes `block` itself;
                                    // checking pre-insert vs post-insert is equivalent, and
                                    // pre-insert lets a genuine mismatch still gate admission
                                    // in enforce mode (the braid has no "undo" once inserted).
                                    let topo_verdict = verify_topology_on_receipt(
                                        br,
                                        bheight,
                                        block.header.topology_commitment,
                                        live_blocks_witnessed,
                                    );
                                    live_blocks_witnessed = live_blocks_witnessed.saturating_add(1);
                                    topology_stats.record(topo_verdict);
                                    match topo_verdict {
                                        TopoVerdict::Mismatch => {
                                            eprintln!(
                                                "🧶⚠ QTFT topology MISMATCH at height {bheight} from {from} — \
                                                 producer's committed window doesn't match what we recompute \
                                                 over the same window (totals: {}/{}/{}/{}/{} match/mismatch/insufficient/nowindow/incomplete)",
                                                topology_stats.matched, topology_stats.mismatched,
                                                topology_stats.insufficient_history, topology_stats.no_window_yet,
                                                topology_stats.window_incomplete,
                                            );
                                            if topology_enforce {
                                                eprintln!("🧶🚫 QTFT: REJECTING block {bheight} from {from} (SIGIL_TOPOLOGY_ENFORCE=1)");
                                                continue;
                                            }
                                        }
                                        TopoVerdict::InsufficientHistory if !topology_stats.logged_insufficient_once => {
                                            topology_stats.logged_insufficient_once = true;
                                            eprintln!(
                                                "🧶 QTFT topology check: not yet verifying (need {} live blocks witnessed \
                                                 locally before comparing; have {live_blocks_witnessed}) — one-time notice, \
                                                 will start comparing once history fills in",
                                                TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN,
                                            );
                                        }
                                        TopoVerdict::WindowIncomplete if !topology_stats.logged_incomplete_once => {
                                            topology_stats.logged_incomplete_once = true;
                                            eprintln!(
                                                "🧶 QTFT topology check: window has a residency gap at height {bheight} \
                                                 (likely bulk-backfill catch-up racing eviction) — skipping comparison for \
                                                 this block rather than risking a false accusation; one-time notice",
                                            );
                                        }
                                        _ => {}
                                    }
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
                                                // QTFT Path C: prefer a peer we've actually seen relay
                                                // THIS block's producer strand recently, over an
                                                // arbitrary connected peer — that peer is more likely to
                                                // hold the missing ancestry we're gapped on. Falls back to
                                                // "first connected" (today's behavior) when we have no
                                                // affinity signal for anyone yet.
                                                let connected = mgr.connected_peers();
                                                if let Some(peer) = peer_affinity.best_for(&connected, Some(block_producer)) {
                                                    // 2026-08-20 (happysrv OOM investigation): this used
                                                    // to request [chain.height()..=bheight+1] UNCAPPED —
                                                    // when a fresh/reconnecting node is deeply behind (a
                                                    // 334,460-block gap was observed live), that's ONE
                                                    // request for a third of a million FULL blocks
                                                    // (headers_only: false), which OOM-killed a node
                                                    // capped at 8G. Cap the range to the same chunk size
                                                    // the windowed fetch pipeline already uses
                                                    // (`FETCH_CHUNK`, defined above in this fn) — a deep
                                                    // gap now closes incrementally over many requests
                                                    // instead of one unbounded one.
                                                    let req_to = chain.height()
                                                        .saturating_add(FETCH_CHUNK)
                                                        .min(bheight.saturating_add(1));
                                                    let req = BackfillReq {
                                                        from: chain.height(),
                                                        to: req_to,
                                                        headers_only: false,
                                                        codec: 0,
                                                        handshake: Some((*sync_hs).clone()),
                                                    };
                                                    // 2026-08-19 (deep-catchup freeze investigation): this used to fire
                                                    // unconditionally every time the 15ms request-throttle allowed a
                                                    // new request through — up to ~66/s, continuously, for the ENTIRE
                                                    // duration of a deep catch-up (which can be minutes). strace on a
                                                    // frozen node showed 74% of all syscall time in futex (lock
                                                     // contention) and 5,069 write() calls in 8 seconds — Rust's
                                                    // stdout is internally mutex-protected, so many threads each
                                                    // hitting eprintln! at a high combined rate is a real livelock
                                                    // mechanism, not just noisy logs. Gate this one to roughly 1/sec
                                                    // (same spirit as the `dag_rejected % 1000` throttle just below
                                                    // for the sibling reject-path print) — still enough to see catch-up
                                                    // is progressing, far too little to contend a lock.
                                                    dag_missing_parents_logged += 1;
                                                    if dag_missing_parents_logged % 64 == 1 {
                                                        eprintln!("⇪ rr-backfill(braid): missing parents at H={} — requesting [{}..={}] from {} ({} such requests so far)",
                                                            bheight, req.from, req.to, peer, dag_missing_parents_logged);
                                                    }
                                                    // Bounded concurrency (see gap_fetch_permits' doc
                                                    // above): if all permits are held, skip firing this
                                                    // tick rather than pile on another in-flight fetch —
                                                    // the 15ms rate-throttle above will let us try again
                                                    // shortly, and a permit frees up as soon as any
                                                    // in-flight fetch completes.
                                                    if let Ok(permit) = std::sync::Arc::clone(&gap_fetch_permits).try_acquire_owned() {
                                                        let mgr2 = std::sync::Arc::clone(&mgr);
                                                        let bf_tx2 = bf_tx.clone();
                                                        tokio::spawn(async move {
                                                            let _permit = permit; // held until this task ends
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
                                        &mut |braw| { let _ = chain_log.append_bytes(braw); },
                                        &send_bridge, &bridge_bridge, &dex_bridge, &usds_bridge, &usds_polygon_bridge,
                                        &shielded_bridge, &mut mint_hash_to_tx_hashes);
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
                                    let braw = chain_log::encode_record(&b).unwrap_or_default();
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
                                        let (req_from, req_to) = (req.from, req.to);
                                        tokio::spawn(async move {
                                            // 2026-08-19 (deep-catchup stall investigation): same
                                            // silent-failure gap as the windowed pipeline above —
                                            // every `if let Ok(...)` here dropped the request on
                                            // the floor with zero trace on any failure, which is
                                            // why a permanently-repeating, permanently-unanswered
                                            // request for the same range was invisible until read
                                            // by hand from the source.
                                            match serde_json::to_vec(&req) {
                                                Ok(payload) => match mgr2.send_request(peer, payload).await {
                                                    Ok(bytes) => match bincode::deserialize::<BackfillResp>(&bytes) {
                                                        Ok(resp) => {
                                                            if resp.blocks.is_empty() {
                                                                eprintln!("⚠ rr-backfill(peer-heights): peer {peer} returned an EMPTY response for [{req_from}..={req_to}) — the repeating-request-no-progress symptom");
                                                            }
                                                            let _ = bf_tx2.send(resp.blocks).await;
                                                        }
                                                        Err(e) => eprintln!("⚠ rr-backfill(peer-heights): decode failed for [{req_from}..={req_to}) from {peer}: {e}"),
                                                    },
                                                    Err(e) => eprintln!("⚠ rr-backfill(peer-heights): send_request failed for [{req_from}..={req_to}) to {peer}: {e}"),
                                                },
                                                Err(e) => eprintln!("⚠ rr-backfill(peer-heights): request serialize failed: {e}"),
                                            }
                                        });
                                    }
                                }
                            } else if topic == sigil_net::TOPIC_TXS {
                                // A transaction arriving already fluffed (either a Dandelion
                                // stem hop chose to fluff it, or the 30s failsafe did). Hand
                                // it to the Dandelion actor for dedup + local apply — do NOT
                                // re-publish; gossipsub's own mesh already fans this out to
                                // our peers (see dandelion_relay.rs's Cmd::FluffIncoming
                                // handling for why re-publishing here would be redundant).
                                //
                                // `data` is a bincode-encoded RelayedTx (Legacy SignedTx JSON
                                // OR a shielded op) — self-describing, so parse-then-forward
                                // rather than assuming SignedTx, matching TOPIC_BLOCKS's own
                                // "reject what doesn't parse" pattern above.
                                if diverged { continue; }
                                if bincode::deserialize::<crate::dandelion_relay::RelayedTx>(&data).is_ok() {
                                    let id = crate::dandelion_relay::id_of(&data);
                                    let _ = dandelion_tx.send(crate::dandelion_relay::Cmd::FluffIncoming { id, bytes: data });
                                }
                            } else if topic == sigil_net::TOPIC_FINALITY_VOTES {
                                // Phase 2: tally only. `on_gossip` returns a log line
                                // for a certificate or an equivocation and `None` for
                                // everything else — including rejected votes, which on
                                // a public topic are the system working, not an
                                // incident. Nothing here reaches Braid::insert().
                                if let Some(line) = finality.on_gossip(&data, finality_wire::now_ms()) {
                                    eprintln!("{}", line);
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
                            // Zero-config WireGuard mesh (wg_relay.rs): record their IP
                            // (needed later to build their WG endpoint) and offer them our
                            // Hello — best-effort, additive, never touches the direct
                            // connection this event itself is reporting on.
                            if let Some(wg) = wg_state.as_ref() {
                                crate::wg_relay::note_peer_ip(wg, peer_id, &addr);
                                let wg2 = Arc::clone(wg);
                                let mgr2 = std::sync::Arc::clone(&mgr);
                                tokio::spawn(async move {
                                    let hello = crate::wg_relay::our_hello(&wg2);
                                    let Ok(payload) = bincode::serialize(&hello) else { return };
                                    if let Ok(bytes) = mgr2.send_request(peer_id, payload).await {
                                        if let Ok(their_hello) = bincode::deserialize::<crate::wg_relay::WgHelloMsg>(&bytes) {
                                            crate::wg_relay::handle_hello(&wg2, peer_id, &their_hello);
                                        }
                                    }
                                });
                            }
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
                            // 2026-08-20: verify_at_height — same early/cheap upgrade
                            // as the braid-arm precheck above.
                            if block.header.verify_at_height(block.header.height).is_err() { continue; }
                            let h = block.header.height;
                            pending_insert(&mut pending, chain.height(), h, block);
                        }
                        while let Some(b) = pending.remove(&chain.height()) {
                            let bhash = b.hash();
                            let view = BlockView::from(&b.header);
                            let braw = chain_log::encode_record(&b).unwrap_or_default();
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
                            &mut |braw| { let _ = chain_log.append_bytes(braw); },
                            &send_bridge, &bridge_bridge, &dex_bridge, &usds_bridge, &usds_polygon_bridge,
                            &shielded_bridge, &mut mint_hash_to_tx_hashes);
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
                            let braw = chain_log::encode_record(&b).unwrap_or_default();
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
mod snapshot_cli;
pub(crate) use snapshot_cli::{run_snapshot_create, run_snapshot_info};

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
        // Roundtrip-hash assertion: if the wire encoding is lossy, the re-parsed
        // block's header will hash to a different value than the local block —
        // which would silently drop blocks on the receiver side. Catch it here.
        // Uses the same tolerant decoder the real receive path does, so this
        // check exercises exactly what a peer would run.
        let parsed: Block = chain_log::decode_record(&block_bytes)
            .context("dry-run: parsing serialized block back off the wire")?;
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
        // build_block_at has no live Braid/DAG context — informational field.
        topology_commitment: None,
    };

    Ok(Block { header, transition, events })
}

// 2026-08-23: DEMO_WALLET / DEMO_INITIAL_BALANCE / GENESIS_AI_ENDOWMENT /
// GENESIS_AI_WALLETS / MASTER_WALLET_GENESIS / GENESIS_TIMESTAMP_MS / build_genesis()
// moved to `genesis.rs` so sigil-top's `producer` feature can share the REAL genesis
// construction instead of a hand-ported duplicate. Re-exported here so every existing
// main.rs call site keeps compiling unchanged.
use genesis::{
    build_genesis, DEMO_INITIAL_BALANCE, DEMO_WALLET, GENESIS_AI_ENDOWMENT, GENESIS_AI_WALLETS,
    GENESIS_TIMESTAMP_MS, MASTER_WALLET_GENESIS,
};
// 2026-08-23: dag_seed_braid / dag_build_frontier / dag_drain_apply / pending_insert /
// dag_store_body / prune_mint_hash_tracking / compute_topology_commitment /
// window_is_complete / topology_commit_hash + their consts moved to `dag.rs`;
// mint_next_block moved to `mint.rs`. Re-exported here so every existing main.rs
// call site keeps compiling unchanged — see `producer/dag.rs` and `producer/mint.rs`
// in sigil-top for the shared consumer this move exists for.
use dag::{
    compute_topology_commitment, dag_drain_apply, dag_seed_braid,
    dag_store_body, pending_insert, prune_mint_hash_tracking, topology_commit_hash,
    window_is_complete, MINT_HASH_TRACKING_CAP, PENDING_MAX_AHEAD, PENDING_MAX_ENTRIES,
    TOPOLOGY_COMMITMENT_WINDOW,
};
use mint::mint_next_block;
// 2026-08-26 REVERTED (production stall, this session): the frontier-memo
// adoption above this comment stalled the live producer within ~2 minutes of
// boot — zero blocks minted, sustained high CPU, `/mining/challenge` 503ing —
// the EXACT symptom `frontier.rs`'s own module doc records for the 2026-08-23
// attempt at this same optimization ("stopped block production dead... rolled
// back"). Confirmed by reverting this one call site back to the plain,
// O(window) `dag_build_frontier` with no other change: production resumed.
// `dag_build_frontier_memo` is NOT deleted — it stays chronos-validated and
// available in `frontier.rs` for a slower, deliberate re-adoption once
// whatever broke it this time (very possibly its interaction with the
// separate, uncommitted `evict_stale_pending` braid.rs work sitting in this
// same tree today) is understood. Re-adopting it live without that
// understanding would just reproduce tonight's outage a third time.
use frontier::dag_build_frontier;

// near_miss_credit / SOLVE_SCAN_MAX / take_creditable_solve now live ONLY in
// solve_credit.rs (shared with sigil-top's producer) — see that module.
// Dual-declared into the binary crate too (same pattern as mint/dag/coinbase),
// so the live producer here calls the SAME implementation, never a duplicate.
mod solve_credit;
pub(crate) use solve_credit::take_creditable_solve;

/// Build (or REBUILD — the wedge self-heal) a braid seeded from the local
/// chain's in-RAM window. A pruned window (base > 0) anchors the braid at the
/// oldest in-RAM block via `Braid::new_with_base` — trusted because this node
/// applied it through the state chokepoint — so seeds chain cleanly instead
/// of parking against unknown pre-window ancestry.
// dag_seed_braid / dag_build_frontier / dag_drain_apply moved to dag.rs (2026-08-23) —
// see the `use dag::{...}` re-export near their old call sites.

/// This process's real memory ceiling, in bytes — cgroup v2, then v1, then
/// total system RAM as the fallback.
///
/// Exists so the serve caches size themselves OUT OF THE BOX. The producer runs
/// under a systemd cgroup with `MemoryHigh`/`MemoryMax`, and exceeding
/// `MemoryHigh` does not merely slow it down: the kernel parks the process in
/// UNINTERRUPTIBLE SLEEP (`mem_cgroup_handle_over_high`). Observed live
/// 2026-08-23 — block production stopped, 174 inbound connections went
/// unaccepted, every client reported `peers 0`, and nothing was logged for
/// minutes, all while the host still had 25 GiB free. A cache budget derived
/// from the ceiling the kernel will actually enforce is the difference between
/// "sized for this machine" and "sized for the machine the developer had".
/// Accumulates elapsed time into a counter when dropped — so a phase that has
/// many early-`continue` exit paths (like the serve arm) is still measured
/// correctly on every path, including the throttled ones.
struct ServeTimer<'a> {
    start: std::time::Instant,
    acc: &'a std::sync::atomic::AtomicU64,
}
impl Drop for ServeTimer<'_> {
    fn drop(&mut self) {
        self.acc.fetch_add(
            self.start.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

mod mem_ceiling;
pub(crate) use mem_ceiling::detect_memory_ceiling_bytes;

/// Extra live blocks (beyond the raw `TOPOLOGY_COMMITMENT_WINDOW`) this node
/// must have personally witnessed via the gossipsub live-block path before
/// `verify_topology_on_receipt` will compare anything. Pure slack against
/// off-by-one boundary effects right at the window edge; not load-bearing.
const TOPOLOGY_VERIFY_HISTORY_MARGIN: u64 = 8;

/// QTFT-2 receipt-side outcome. `InsufficientHistory`, `NoWindowYet`, and
/// `WindowIncomplete` are all "nothing to compare" — distinguished only for
/// logging/telemetry, and NEVER treated as a mismatch (a node with too
/// little, or too gappy, local history has no basis to accuse an honest
/// peer). `WindowIncomplete` was added 2026-08-19/20 after a real incident:
/// a node that catches up via bulk backfill (not one-at-a-time live gossip)
/// reached `InsufficientHistory`'s live-witness threshold while its DAG
/// window for the checked heights was still gappy (eviction racing the
/// catch-up), and every single check came back Mismatch — 100% failure rate
/// from the first eligible block, on a chain independently confirmed healthy
/// (no state-root divergence, clean apply). `blocks_witnessed_live` alone
/// was not a sufficient proxy for "my window is trustworthy"; this adds the
/// direct check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopoVerdict {
    Match,
    Mismatch,
    InsufficientHistory,
    NoWindowYet,
    WindowIncomplete,
}

/// Running counters for `verify_topology_on_receipt`, surfaced in logs (and
/// available to wire into `/v1/status` later if useful). `logged_insufficient_once`
/// keeps the boot-time "still filling in history" notice to a single line
/// instead of one per block during the fill-in period; `logged_incomplete_once`
/// does the same for the window-gap case.
#[derive(Debug, Default)]
struct TopologyStats {
    matched: u64,
    mismatched: u64,
    insufficient_history: u64,
    no_window_yet: u64,
    window_incomplete: u64,
    logged_insufficient_once: bool,
    logged_incomplete_once: bool,
}

impl TopologyStats {
    fn record(&mut self, v: TopoVerdict) {
        match v {
            TopoVerdict::Match => self.matched += 1,
            TopoVerdict::Mismatch => self.mismatched += 1,
            TopoVerdict::InsufficientHistory => self.insufficient_history += 1,
            TopoVerdict::NoWindowYet => self.no_window_yet += 1,
            TopoVerdict::WindowIncomplete => self.window_incomplete += 1,
        }
    }
}

/// Bound on `PeerProducerAffinity::seen` — a soft preference hint, not
/// correctness-critical state, so eviction on overflow doesn't need to be
/// precise (see `record`).
const PEER_AFFINITY_CAP: usize = 4096;

/// QTFT Path C, SIGIL-level v1 (see `SIGIL_QTFT_TOPOLOGY_v0.md`'s "knot-
/// routing in p2p" idea). The doc's original framing was a generic scoring
/// hook inside `flux-p2p` itself; that would mean teaching a chain-agnostic
/// transport crate about producers/braids, which conflicts with its own
/// "zero chain deps" boundary (mirroring `flux-topology`'s). This delivers
/// the real behavioral win at the layer that actually has both the p2p peer
/// list AND the braid/producer context: `sigil-node`.
///
/// The idea: strands that have recently crossed (merged) in the braid are
/// "topologically adjacent" — a peer relaying one is plausibly tracking the
/// other too. Concretely: remember which peer most recently delivered a
/// LIVE block from which producer, and when backfilling a gap for a known
/// producer, prefer a peer with a recent sighting of that exact producer
/// over an arbitrary connected peer.
#[derive(Default)]
struct PeerProducerAffinity {
    /// (peer, producer) → height of the most recent live block we saw that
    /// peer relay from that producer.
    seen: std::collections::HashMap<(flux_p2p::PeerId, [u8; 32]), u64>,
}

impl PeerProducerAffinity {
    fn record(&mut self, peer: flux_p2p::PeerId, producer: [u8; 32], height: u64) {
        let key = (peer, producer);
        if self.seen.len() >= PEER_AFFINITY_CAP && !self.seen.contains_key(&key) {
            // Soft bound: drop an arbitrary entry rather than track proper
            // LRU order — this only ever biases a peer preference, it never
            // gates correctness, so an imprecise evict is fine.
            if let Some(k) = self.seen.keys().next().copied() {
                self.seen.remove(&k);
            }
        }
        self.seen
            .entry(key)
            .and_modify(|h| *h = (*h).max(height))
            .or_insert(height);
    }

    /// Prefer a connected peer with a recent sighting of `producer`;
    /// otherwise fall back to the first connected peer (today's behavior).
    fn best_for(&self, connected: &[flux_p2p::PeerId], producer: Option<[u8; 32]>) -> Option<flux_p2p::PeerId> {
        if let Some(p) = producer {
            let mut best: Option<(flux_p2p::PeerId, u64)> = None;
            for peer in connected {
                if let Some(&h) = self.seen.get(&(*peer, p)) {
                    if best.map(|(_, bh)| h > bh).unwrap_or(true) {
                        best = Some((*peer, h));
                    }
                }
            }
            if let Some((peer, _)) = best {
                return Some(peer);
            }
        }
        connected.first().copied()
    }
}

#[cfg(test)]
mod peer_affinity_tests {
    use super::PeerProducerAffinity;
    use flux_p2p::PeerId;

    #[test]
    fn best_for_prefers_the_most_recent_producer_sighting_else_first() {
        let (a, b, c) = (PeerId::random(), PeerId::random(), PeerId::random());
        let prod = [7u8; 32];
        let mut aff = PeerProducerAffinity::default();

        // No sightings → fall back to the FIRST connected peer.
        assert_eq!(aff.best_for(&[a, b], Some(prod)), Some(a));
        // producer = None → no affinity to use → first connected peer.
        assert_eq!(aff.best_for(&[b, a], None), Some(b));
        // Nothing connected → None.
        assert_eq!(aff.best_for(&[], Some(prod)), None);

        // b has seen `prod` (h=10), a hasn't → prefer b even though a is first.
        aff.record(b, prod, 10);
        assert_eq!(aff.best_for(&[a, b], Some(prod)), Some(b));

        // c saw it more recently (h=20) → the HIGHEST-height sighting wins.
        aff.record(c, prod, 20);
        assert_eq!(aff.best_for(&[a, b, c], Some(prod)), Some(c));

        // A sighting of a DIFFERENT producer doesn't influence this one's choice.
        aff.record(a, [9u8; 32], 999);
        assert_eq!(aff.best_for(&[a, b, c], Some(prod)), Some(c));

        // record keeps the MAX height: b jumps to 25 and now beats c's 20 ...
        aff.record(b, prod, 25);
        assert_eq!(aff.best_for(&[b, c], Some(prod)), Some(b));
        // ... and a later LOWER height must not downgrade it (max-wins).
        aff.record(b, prod, 1);
        assert_eq!(aff.best_for(&[b, c], Some(prod)), Some(b));
    }
}

/// QTFT-2: recompute the topology commitment a peer's block SHOULD carry —
/// using the exact same windowed Alexander-polynomial algorithm the producer
/// used at mint time (`compute_topology_commitment`) — and compare it to
/// what the block actually claims.
///
/// Called BEFORE the block is admitted to the braid (see call site), over
/// the window `[height-32, height-1]`: strictly this block's ANCESTORS, so
/// it is well-defined whether or not `block` itself has been inserted yet.
///
/// Deliberately refuses to render a verdict until this node has personally
/// witnessed (via the live gossipsub path — never via bulk backfill/snapshot
/// restore, which could hand it a partial or differently-sourced window)
/// at least `TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN`
/// blocks since boot. Below that threshold this node's own window may be
/// incomplete relative to what the producer saw, which would manufacture
/// false mismatches against perfectly honest peers — so it reports
/// `InsufficientHistory` instead of guessing.
fn verify_topology_on_receipt(
    braid: &Braid,
    height: u64,
    claimed: Option<[u8; 32]>,
    blocks_witnessed_live: u64,
) -> TopoVerdict {
    if height == 0 {
        return TopoVerdict::NoWindowYet;
    }
    if blocks_witnessed_live < TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN {
        return TopoVerdict::InsufficientHistory;
    }
    // Check window completeness directly rather than trusting
    // `blocks_witnessed_live` as a proxy for it — a node that caught up via
    // bulk backfill can cross the live-witness threshold while its window
    // for THESE heights still has eviction gaps (see TopoVerdict's doc).
    let to_height = height - 1;
    let from_height = to_height.saturating_sub(TOPOLOGY_COMMITMENT_WINDOW.saturating_sub(1));
    let bp = braid.braid_word(from_height, to_height);
    if !window_is_complete(&bp, from_height, to_height) {
        return TopoVerdict::WindowIncomplete;
    }
    let bw = flux_topology::BraidWord { strands: bp.strands, gens: bp.word.clone() };
    let delta = flux_topology::alexander_poly(&bw);
    let expected = topology_commit_hash(&delta, bp.strands, &bp.word, &bp.producers);
    if expected == claimed {
        TopoVerdict::Match
    } else {
        TopoVerdict::Mismatch
    }
}

// mint_next_block() moved to mint.rs (2026-08-23) — see the `use mint::mint_next_block`
// near DEMO_WALLET's old site above.

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

#[cfg(test)]
mod listen_addr_tests {
    use super::resolve_listen_addr;
    use sigil_net::SigilTransport;
    #[test]
    fn only_direct_binds_public() {
        // SECURITY invariant: Direct exposes 0.0.0.0 (the node is meant to be dialed);
        // Tor stays on 127.0.0.1. A regression that bound a private transport to 0.0.0.0
        // would silently expose a node meant to be reachable only over its tunnel.
        assert_eq!(resolve_listen_addr(&SigilTransport::Direct, 9501), "/ip4/0.0.0.0/tcp/9501");
        assert_eq!(resolve_listen_addr(&SigilTransport::Tor, 9501), "/ip4/127.0.0.1/tcp/9501");
    }
}

/// Env var overrides for `wg-up`. The defaults work for single-machine
/// dev mode; multi-node operators MUST set SIGIL_WG_ADDRESS per node or
/// the mesh will silently overlap on `10.42.0.1/16`.
const WG_LISTEN_PORT_ENV: &str = "SIGIL_WG_LISTEN_PORT";
const WG_ADDRESS_ENV: &str     = "SIGIL_WG_ADDRESS";

mod wg_cli;
pub(crate) use wg_cli::{run_wg_add_peer, run_wg_down, run_wg_list_peers, run_wg_up};

// hex_full + hex_short_block extracted to hex_fmt.rs (god-file split, 2026-09-01),
// re-exported so all call sites are unchanged.
mod hex_fmt;
pub(crate) use hex_fmt::{hex_full, hex_short_block};
mod wg_manifest;
pub(crate) use wg_manifest::{
    load_or_generate_wg_key, load_peers_manifest, peers_manifest_path, save_peers_manifest,
};

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

// hex_short_block moved to hex_fmt.rs (see the `mod hex_fmt` re-export above).

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
        let (block, _included_tx_hashes) = mint_next_block(&chain, Vec::new(), &[], None, Some(&solve), None, None)
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

        // g2 genesis carries NO premine (supply at H=0 is 0 — see genesis.rs), so
        // DEMO_WALLET starts empty and nothing could be spent. Credit it in the
        // staging state, standing in for the coinbase that funds a spender on the
        // real chain, so the Send has balance to move. This is a pipeline test
        // (does producing a block with a real signed tx advance the tip), not a
        // conservation test — that lives in sigil-tx / the coinbase suite.
        let mut staging = chain.state_snapshot();
        let seed = StateTransition {
            at_height: 1,
            mutations: vec![sigil_state::StateMutation::SetBalance {
                wallet: DEMO_WALLET,
                token: [0u8; 32],
                amount: DEMO_INITIAL_BALANCE,
            }],
        };
        sigil_state::commit_state_transition(&mut staging, &seed, 1).unwrap();
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
    fn self_mined_block_is_really_signed_and_verifies_at_activation() {
        // The full, REAL path Epsilon runs once SIGIL_PRODUCER_SIGNING_SEED_HEX
        // is configured: mint_next_block (production code, solve=None — the
        // self-mined/no-external-miner path) → the resulting header must
        // carry a genuine Ed25519 signature that verify_at_height() accepts
        // AT the real activation height (H1_PRODUCER_SIG_ACTIVATION_HEIGHT).
        // Not a unit test of the crypto in isolation — this exercises the
        // exact function the live node calls.
        let _guard = crate::producer_signing::locked();
        let seed = [42u8; 32];
        std::env::set_var("SIGIL_PRODUCER_SIGNING_SEED_HEX", hex::encode(seed));

        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).unwrap();

        let (block, _included) = mint_next_block(&chain, vec![], &[], None, None, None, None)
            .expect("self-mined block should mint");

        assert_eq!(block.header.sig_scheme, SigScheme::Ed25519Hot, "self-mined + configured key => real scheme");
        assert_ne!(block.header.producer, [0u8; 32], "producer must be the configured wallet, not the old placeholder");
        block.header.verify_producer_sig().expect("mint_next_block's signature must actually verify");
        block
            .header
            .verify_at_height(sigil_header::H1_PRODUCER_SIG_ACTIVATION_HEIGHT)
            .expect("must pass verify_at_height at the real, configured activation height");

        // And it applies cleanly through the real chain, same as any block.
        chain.apply(block).expect("signed self-mined block applies normally");
        assert_eq!(chain.height(), 2);

        std::env::remove_var("SIGIL_PRODUCER_SIGNING_SEED_HEX");
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
            let (b, _included_tx_hashes) = mint_next_block(&builder, vec![], &[], None, None, None, None).expect("mint");
            blocks.push(b.clone());
            builder.apply(b).expect("apply");
        }

        // Follower: fresh chain + braid with final_depth 0 so every ordered
        // block finalizes at the tip (the wiring under test, not finality lag).
        let mut chain = ChainTip::new();
        let mut braid = Braid::new(BraidConfig { final_depth: 0, ..BraidConfig::default() });
        let mut dag_bodies = std::collections::HashMap::new();
        let mut mint_hash_to_tx_hashes = std::collections::HashMap::new();
        let send_bridge = sigil_api::send::SendBridge::new();
        let bridge_bridge = sigil_api::bridge::BridgeBridge::new(None, None);
        let dex_bridge = sigil_api::dex::DexBridge::new();
        let usds_bridge = sigil_api::usds::UsdsBridge::new();
        let usds_polygon_bridge = sigil_api::usds_bridge::UsdsBridgeBridge::new(None, None);
        let shielded_bridge = sigil_api::shielded::ShieldedBridge::new();
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
            }, &send_bridge, &bridge_bridge, &dex_bridge, &usds_bridge, &usds_polygon_bridge,
               &shielded_bridge, &mut mint_hash_to_tx_hashes);
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
            .map(|raw| chain_log::decode_record(raw).unwrap().header.height)
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

    // ── QTFT-2: receipt-side topology verification ──────────────────────────

    /// Synthetic deterministic hash for a test height (offset by 1 so height
    /// 0's hash never collides with the all-zero genesis parent).
    fn qtft_test_hash(n: u64) -> BlockHash {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&(n + 1).to_le_bytes());
        b
    }

    const QTFT_PA: [u8; 32] = [0xAA; 32];
    const QTFT_PB: [u8; 32] = [0xBB; 32];
    const QTFT_PC: [u8; 32] = [0xCC; 32];

    /// A 3-producer synthetic braid — deliberately 3, not 2: with only 2
    /// strands the arrangement is always already-adjacent (see
    /// `sigil_dagknight::present`'s own `adjacent_strand_merge_yields_no_crossing`
    /// test), so a 2-producer braid word is ALWAYS empty no matter how many
    /// blocks merge — the invariant only ever varies with ≥3 real producers.
    /// Round-robins PA/PB/PC as producer and merges 2-back (a different
    /// strand two-thirds of the time) to generate real crossings throughout.
    fn qtft_test_views(n: u64) -> Vec<BlockView> {
        (0..n)
            .map(|height| {
                let producer = match height % 3 {
                    0 => QTFT_PA,
                    1 => QTFT_PB,
                    _ => QTFT_PC,
                };
                let parent = if height == 0 { [0u8; 32] } else { qtft_test_hash(height - 1) };
                let merge_parents = if height >= 2 { vec![qtft_test_hash(height - 2)] } else { vec![] };
                BlockView {
                    hash: qtft_test_hash(height),
                    parent,
                    merge_parents,
                    height,
                    producer,
                    // Free-run mint: no PoW solve behind these synthetic blocks,
                    // which is what `difficulty = 0` means on this chain (see BlockView::difficulty).
                    difficulty: 0,
                }
            })
            .collect()
    }

    #[test]
    fn topology_verify_matches_honest_producer_once_history_is_sufficient() {
        let n = TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN + 10;
        let views = qtft_test_views(n);

        // "Sender": mirrors real mint-time order exactly — compute the
        // commitment for height h over ancestors ONLY, THEN insert h's own
        // view (so later heights' windows can see it as an ancestor).
        let mut sender = Braid::new(BraidConfig::default());
        let mut claimed: Vec<Option<[u8; 32]>> = Vec::with_capacity(n as usize);
        for (h, view) in views.iter().enumerate() {
            claimed.push(compute_topology_commitment(Some(&sender), h as u64));
            assert!(!matches!(sender.insert(view.clone()), InsertOutcome::Rejected(_)));
        }

        // "Receiver": a completely independent Braid, fed the identical
        // views in the identical order, exercising the exact call sequence
        // production code uses (verify BEFORE insert).
        let mut receiver = Braid::new(BraidConfig::default());
        let mut witnessed = 0u64;
        let threshold = TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN;
        let mut saw_a_real_crossing = false;
        for (h, view) in views.iter().enumerate() {
            let verdict = verify_topology_on_receipt(&receiver, h as u64, claimed[h], witnessed);
            match h as u64 {
                0 => assert_eq!(verdict, TopoVerdict::NoWindowYet, "genesis has no window"),
                h if h < threshold => assert_eq!(
                    verdict, TopoVerdict::InsufficientHistory,
                    "height {h}: receiver hasn't witnessed a full window yet"
                ),
                h => assert_eq!(
                    verdict, TopoVerdict::Match,
                    "height {h}: identical append order must recompute identically"
                ),
            }
            witnessed += 1;
            assert!(!matches!(receiver.insert(view.clone()), InsertOutcome::Rejected(_)));
        }
        // Sanity: with 3 real producers merging across strands, at least one
        // window's commitment must differ from height 1's (i.e. the field
        // genuinely varies — not silently degenerate the way a 2-producer
        // braid always is).
        for h in (threshold as usize)..(n as usize) {
            if claimed[h] != claimed[threshold as usize] {
                saw_a_real_crossing = true;
                break;
            }
        }
        assert!(saw_a_real_crossing, "3-producer braid must produce a non-constant topology commitment somewhere in the window");
    }

    #[test]
    fn topology_verify_catches_a_genuinely_tampered_commitment() {
        let n = TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN + 5;
        let views = qtft_test_views(n);

        let mut sender = Braid::new(BraidConfig::default());
        let mut claimed: Vec<Option<[u8; 32]>> = Vec::with_capacity(n as usize);
        for (h, view) in views.iter().enumerate() {
            claimed.push(compute_topology_commitment(Some(&sender), h as u64));
            assert!(!matches!(sender.insert(view.clone()), InsertOutcome::Rejected(_)));
        }

        // Tamper with the LAST block's claimed commitment — simulating a
        // dishonest or buggy peer whose header doesn't match its own braid.
        let tamper_height = (n - 1) as usize;
        let tampered = match claimed[tamper_height] {
            Some(mut bytes) => { bytes[0] ^= 0xFF; Some(bytes) }
            None => Some([0x99u8; 32]),
        };
        claimed[tamper_height] = tampered;

        let mut receiver = Braid::new(BraidConfig::default());
        let mut witnessed = 0u64;
        let mut last_verdict = TopoVerdict::NoWindowYet;
        for (h, view) in views.iter().enumerate() {
            last_verdict = verify_topology_on_receipt(&receiver, h as u64, claimed[h], witnessed);
            witnessed += 1;
            assert!(!matches!(receiver.insert(view.clone()), InsertOutcome::Rejected(_)));
        }
        assert_eq!(last_verdict, TopoVerdict::Mismatch, "a genuinely tampered commitment must be caught");
    }

    #[test]
    fn topology_commit_hash_distinguishes_braids_that_share_an_alexander_polynomial() {
        // Both windows below have Alexander polynomial Δ=1 — the unknot's
        // value. `flux_topology`'s own KAT tests already establish this for
        // BOTH cases independently: `kat_unknot_sigma1_n2_delta_is_1` proves
        // a single crossing on 2 strands closes to the unknot, and
        // `kat_sigma1_sigma2_unknot_closure` proves σ1σ2 on 3 strands ALSO
        // closes to the unknot — same Δ, different strand count AND word.
        // (NOTE: the truly EMPTY word on 2 strands is NOT a third example of
        // this — it closes to a 2-component UNLINK, Δ=0, not the unknot; an
        // earlier version of this test wrongly assumed otherwise and the
        // premise assertion below caught it immediately.) A commitment over
        // the polynomial alone could not tell the two real examples below
        // apart — which is exactly the collision gap this hash construction
        // exists to close. Prove it actually does.
        let two_strand = flux_topology::BraidWord::new(2, vec![1]);
        let delta_two_strand = flux_topology::alexander_poly(&two_strand);
        let non_trivial = flux_topology::BraidWord::new(3, vec![1, 2]);
        let delta_non_trivial = flux_topology::alexander_poly(&non_trivial);
        assert_eq!(delta_two_strand, delta_non_trivial, "test premise: both must share Δ=1");

        let producers_2 = [[0xAAu8; 32], [0xBBu8; 32]];
        let producers_3 = [[0xAAu8; 32], [0xBBu8; 32], [0xCCu8; 32]];
        let c1 = topology_commit_hash(&delta_two_strand, two_strand.strands, &two_strand.gens, &producers_2);
        let c2 = topology_commit_hash(&delta_non_trivial, non_trivial.strands, &non_trivial.gens, &producers_3);
        assert_ne!(
            c1, c2,
            "two distinct braid presentations sharing an Alexander polynomial must still commit differently"
        );

        // Sanity: the SAME presentation must still commit identically
        // (determinism, not just difference-sensitivity).
        let c1_again = topology_commit_hash(&delta_two_strand, two_strand.strands, &two_strand.gens, &producers_2);
        assert_eq!(c1, c1_again, "identical input must hash identically");
    }

    #[test]
    fn topology_verify_never_flags_insufficient_history_as_a_mismatch() {
        // Below the witnessed-history threshold, verify_topology_on_receipt
        // must NEVER return Mismatch even when handed a wildly wrong claim —
        // an honest peer must never be accused because OUR local history is
        // thin (fresh boot / recent snapshot restore).
        let braid = Braid::new(BraidConfig::default());
        let verdict = verify_topology_on_receipt(&braid, 5, Some([0xEEu8; 32]), 0);
        assert_eq!(verdict, TopoVerdict::InsufficientHistory);
    }

    #[test]
    fn topology_verify_never_flags_a_gappy_window_as_a_mismatch() {
        // Real incident (2026-08-20, happysrv): a node that caught up via
        // bulk backfill crossed the InsufficientHistory threshold while its
        // window for the checked heights was NOT fully resident (a gap from
        // eviction racing the catch-up) — and every single check came back
        // Mismatch, on a chain independently confirmed healthy. Reproduce
        // the gap directly: seed a braid anchored at height 100 (so heights
        // below 100 are simply never resident — no view, no data), insert
        // views only from height 101 onward, then check at a height whose
        // 32-block window reaches back into the un-seeded range.
        let base_height = 100u64;
        let base_hash = qtft_test_hash(base_height);
        let mut braid = Braid::new_with_base(BraidConfig::default(), base_hash, base_height);

        // 3 producers so the word isn't trivially degenerate (see the other
        // QTFT tests' note on why 2 strands can't test this meaningfully).
        for height in (base_height + 1)..=(base_height + 20) {
            let producer = match height % 3 {
                0 => QTFT_PA,
                1 => QTFT_PB,
                _ => QTFT_PC,
            };
            let parent = qtft_test_hash(height - 1);
            let view = BlockView {
                hash: qtft_test_hash(height),
                parent,
                merge_parents: vec![],
                height,
                producer,
                // Free-run mint: no PoW solve behind these synthetic blocks,
                // which is what `difficulty = 0` means on this chain (see BlockView::difficulty).
                difficulty: 0,
            };
            assert!(!matches!(braid.insert(view), InsertOutcome::Rejected(_)));
        }

        // Check at height 110: window = [110-32, 109] = [78, 109]. Heights
        // 78..=100 were never seeded — a real, unavoidable gap. Plenty of
        // "live blocks witnessed" so InsufficientHistory doesn't mask it —
        // this must be caught by the completeness check specifically.
        let witnessed = TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN + 100;
        let verdict = verify_topology_on_receipt(&braid, 110, Some([0xEEu8; 32]), witnessed);
        assert_eq!(
            verdict, TopoVerdict::WindowIncomplete,
            "a gappy window must never be reported as Match or Mismatch — only as incomplete"
        );

        // Sanity: a height whose FULL window is inside the seeded range
        // (e.g. height 120: window [88,119]... still touches the gap since
        // base is 100). Use a height deep enough that the whole 32-block
        // window is past the seed point.
        let deep_height = base_height + TOPOLOGY_COMMITMENT_WINDOW + 5;
        for height in (base_height + 21)..=deep_height {
            let producer = match height % 3 {
                0 => QTFT_PA,
                1 => QTFT_PB,
                _ => QTFT_PC,
            };
            let view = BlockView {
                hash: qtft_test_hash(height),
                parent: qtft_test_hash(height - 1),
                merge_parents: vec![],
                height,
                producer,
                // Free-run mint: no PoW solve behind these synthetic blocks,
                // which is what `difficulty = 0` means on this chain (see BlockView::difficulty).
                difficulty: 0,
            };
            assert!(!matches!(braid.insert(view), InsertOutcome::Rejected(_)));
        }
        let verdict2 = verify_topology_on_receipt(&braid, deep_height, Some([0xEEu8; 32]), witnessed);
        // This one's window is fully resident, so it CAN render a real
        // verdict now — it'll be Mismatch (claimed value is a dummy), which
        // is the correct, expected outcome once completeness is satisfied.
        assert_eq!(verdict2, TopoVerdict::Mismatch, "a complete window with a wrong claim IS a real mismatch");
    }
}
