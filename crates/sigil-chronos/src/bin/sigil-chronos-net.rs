//! sigil-chronos-net — run a real SIGIL SimNode over real flux-p2p.
//!
//! The CHRONOS-T wire twin of the in-sim soak. Same `SigilSimNode` chain
//! logic; real libp2p gossipsub instead of the in-memory Universe bus.
//!
//! ```text
//! # Delta (producer, listens):
//! SIGIL_LISTEN=/ip4/0.0.0.0/tcp/9501 sigil-chronos-net producer --blocks 50
//!
//! # Epsilon (follower, dials Delta):
//! SIGIL_LISTEN=/ip4/0.0.0.0/tcp/9501 \
//! SIGIL_BOOTSTRAP=/ip4/5.79.79.158/tcp/9501/p2p/<delta-peer-id> \
//!   sigil-chronos-net follower
//! ```
//!
//! Genesis is `sigil_chronos::demo_genesis()` on both sides → shared tip at
//! H=0. Follower logs `apply H=k -> Ok` per block; any `Divergence` is the
//! exit-78 condition crossing the real wire.

use flux_chronos::NodeId;
use flux_p2p::swarm::peer_id_string;
use flux_p2p::{NetworkConfig, NetworkManager};
use sigil_chronos::{demo_genesis, driver, sign_dummy, SigilSimNode};
use sigil_tx::{SigilTx, NATIVE};

const TOPIC_BLOCKS: &str = "/sigil/g0/blocks";

#[tokio::main]
async fn main() {
    // Surface flux-p2p's tracing (local peer id, peer connect/disconnect,
    // gossipsub graft) so the wire run is observable. RUST_LOG overrides.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,flux_p2p=info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let is_producer = match mode {
        "producer" => true,
        "follower" => false,
        _ => {
            eprintln!("usage: sigil-chronos-net <producer|follower> [--blocks N] [--run-secs S]");
            eprintln!("env: SIGIL_LISTEN (multiaddr), SIGIL_BOOTSTRAP (multiaddr, follower)");
            std::process::exit(64);
        }
    };

    let blocks: u32 = arg_val(&args, "--blocks").and_then(|s| s.parse().ok()).unwrap_or(50);
    let run_secs: u64 = arg_val(&args, "--run-secs").and_then(|s| s.parse().ok()).unwrap_or(120);
    let listen = std::env::var("SIGIL_LISTEN").unwrap_or_else(|_| "/ip4/0.0.0.0/tcp/9501".into());
    let bootstrap = std::env::var("SIGIL_BOOTSTRAP").ok();

    // ── flux-p2p config: subscribe to the SIGIL blocks topic so drain_events
    //    surfaces blocks the peer publishes. ──
    let config = NetworkConfig {
        // ROOT-CAUSE FIX (2026-05-31): node_id must be UNIQUE per process. flux-p2p
        // derives the libp2p keypair deterministically from node_id
        // (swarm.rs keypair_from_seed). With a shared "sigil-follower" id, every
        // follower got the SAME peer_id → libp2p dedups by peer_id → N followers
        // collapsed into 1 churning logical peer → gossip mesh never held → 0
        // block propagation. Suffixing the PID gives each node a distinct peer_id.
        node_id: format!("sigil-{mode}-{}", std::process::id()),
        listen_addr: listen.clone(),
        bootstrap_peers: bootstrap.clone().into_iter().collect(),
        dagknight_enabled: false,
        sap_enabled: false,
        x_algo_enabled: false,
        entanglement_enabled: false,
        gossipsub_topics: vec![TOPIC_BLOCKS.to_string()],
    };

    // ROOT-FIX (peer_id capture): write our deterministic peer_id to a file on boot,
    // so a launcher/follower never has to scrape it from the noisy unit journal
    // (which handed back STALE ids across restarts → follower dialed the right
    // address with the wrong identity → 0 peers). Default path is per-mode; override
    // with SIGIL_PEERID_FILE. peer_id_string() is the SAME derivation the swarm uses.
    let peer_id = peer_id_string(&config.node_id);
    let peerid_file = std::env::var("SIGIL_PEERID_FILE")
        .unwrap_or_else(|_| format!("/tmp/sigil-{mode}-peerid"));
    let _ = std::fs::write(&peerid_file, &peer_id);
    println!("SIGIL_PEER_ID={peer_id}");
    eprintln!("   peer_id {peer_id} → {peerid_file}");

    let mut nm = NetworkManager::new(config);
    if let Err(e) = nm.start().await {
        eprintln!("flux-p2p start failed: {e}");
        std::process::exit(1);
    }
    eprintln!("🌐 sigil-chronos-net {mode} — listening {listen}");
    if let Some(b) = &bootstrap {
        eprintln!("   dialing bootstrap {b}");
    }

    // ── Build the node from the shared genesis. ──
    let g = demo_genesis();
    let my_id = NodeId(0);
    // Producer needs ≥1 peer in its list so step() emits a publish envelope
    // the driver captures + broadcasts.
    let peers = if is_producer { vec![NodeId(1)] } else { vec![] };
    let block_time_ms = 1_000;
    let mut node = SigilSimNode::new(mode, my_id, peers, is_producer, block_time_ms * 1_000, &g);

    // ── Producer pre-seeds its mempool with `blocks` Send txs across the
    //    demo wallets. One tx per block → `blocks` blocks. ──
    if is_producer {
        for n in 0..blocks {
            let from = [(n % 5 + 1) as u8; 32];
            let to = [((n + 1) % 5 + 1) as u8; 32];
            node.enqueue_tx(sign_dummy(SigilTx::Send {
                from,
                to,
                amount: 100,
                token: NATIVE,
                fee: 1,
            }));
        }
        eprintln!("   seeded {blocks} Send txs; producing 1 block/{block_time_ms}ms once a peer is up");
    }

    // ── Drive over the real transport. Blocks until run_secs elapse. ──
    let transport = sigil_chronos::transport::RealP2pTransport::new(nm);
    let final_node = driver::run(node, transport, block_time_ms, run_secs);

    eprintln!(
        "📊 {mode} done — blocks_applied={} divergence={} rejected={} height={}",
        final_node.blocks_applied,
        final_node.divergence_count,
        final_node.rejected_count,
        final_node.height(),
    );
    if final_node.divergence_count > 0 {
        std::process::exit(78); // SIGIL exit-78 divergence convention
    }
}

fn arg_val<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}
