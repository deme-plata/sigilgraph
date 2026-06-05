//! chronos_sim.rs — drive REAL deterministic blocks through the flux-db store
//! (sigil_node::store::BlockStore) and measure the ACTUAL on-disk growth of the
//! SIGIL graph, including LSM overhead (SSTs, bloom filters, WAL) that a
//! per-block projection can't see. Writes a results JSON the flux-vite-engine
//! page renders.
//!
//! Blocks come from `sigil_chronos::SigilSimNode::produce_one()` — the real
//! single-producer path (genesis applied, roots computed through the actual
//! state chokepoint). Their Phase-0 crypto witness fields are zeroed
//! placeholders, so we inject high-entropy bytes into them to model production
//! reality (footprint.rs showed the two 292B SQIsign sigs dominate block size;
//! zeroed sigs would let LZ4 cheat).
//!
//! Three storage models measured side by side:
//!   1. JSON snapshot  — serialize the whole accumulated chain (today's path)
//!   2. flux-db full   — bincode block per CF key, LZ4 SSTs (the fix)
//!   3. flux-db pruned — witness-pruned core per CF key (archival floor)

use sigil_chronos::{demo_genesis, sign_dummy, Block, SigilSimNode};
use flux_chronos::NodeId;
use sigil_header::{SignatureBytes, SqiSignature, SQISIGN_L5_LEN};
use sigil_node::store::{BlockStore, PrunedHeader};
use sigil_state::{StateTransition, WalletId};
use sigil_events::SigilEvent;
use sigil_tx::{SigilTx, NATIVE};
use serde::Serialize;

/// History-preserving prune: drop the crypto witness (the two 292B SQIsign
/// sigs + VDF/STARK proofs) but KEEP the transition + events, so the node can
/// still serve full transaction history + inclusion proofs — it just can't
/// re-verify the producer signature from scratch (trusts its past verification).
/// This is the realistic "pruned full node" tier, between full and state-only.
#[derive(Serialize)]
struct HistoryBlock<'a> {
    core: PrunedHeader,
    transition: &'a StateTransition,
    events: &'a [SigilEvent],
}

fn entropy(seed: u64, buf: &mut [u8]) {
    let mut h = blake3::Hasher::new();
    h.update(&seed.to_le_bytes());
    h.update(b"sigil-chronos-sim");
    h.finalize_xof().fill(buf);
}
fn h32(seed: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    entropy(seed, &mut b);
    b
}

/// Replace the zeroed Phase-0 crypto placeholders with high-entropy bytes so
/// the stored block matches production reality (real signatures don't compress).
fn realize(mut b: Block) -> Block {
    let s = b.header.height * 13 + 1;
    let mut sig = [0u8; SQISIGN_L5_LEN];
    entropy(s, &mut sig);
    b.header.nonce_sqisign = SqiSignature::from_array(sig);
    let mut psig = [0u8; SQISIGN_L5_LEN];
    entropy(s + 1, &mut psig);
    b.header.producer_sig = SignatureBytes(psig.to_vec());
    b.header.producer = h32(s + 2);
    b.header.txs_merkle_root = h32(s + 3);
    b.header.fluxc_artifact_proof.artifact_blake3 = h32(s + 4);
    b.header.state_transition_proof.public_inputs_hash = h32(s + 5);
    b
}

fn human(bytes: f64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let (mut v, mut i) = (bytes, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, U[i])
}

fn main() {
    let n: u64 = std::env::var("SIM_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let step: u64 = std::env::var("SIM_STEP").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000);
    let out = std::env::var("SIM_OUT")
        .unwrap_or_else(|_| "/home/storage/deepseek-codewhale/sigil/chronos-footprint-results.json".into());

    // Fresh scratch dirs for the two flux-db stores.
    let base = std::env::temp_dir().join(format!("sigil-chronos-sim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let full = BlockStore::open(base.join("fluxdb-full")).expect("open full store");
    let history = BlockStore::open(base.join("fluxdb-history")).expect("open history store");
    let pruned = BlockStore::open(base.join("fluxdb-pruned")).expect("open pruned store");

    // Real deterministic producer. demo_genesis funds a set of wallets; we
    // round-robin a Send between them each block so the producer has a real tx
    // to mint (plus the coinbase reward) — realistic non-empty transitions.
    let g = demo_genesis();
    let funded: Vec<WalletId> = g.funded.iter().map(|(w, _)| *w).collect();
    assert!(funded.len() >= 2, "need ≥2 funded wallets to drive transfers");
    let mut node = SigilSimNode::new("footprint-producer", NodeId(0), vec![], true, 1_000, &g);

    let mut json_chain: Vec<Block> = Vec::with_capacity(n as usize);
    let mut curve = Vec::new();

    println!("=== chronos footprint sim — producing {} real blocks ===\n", n);
    for h in 1..=n {
        let from = funded[(h as usize) % funded.len()];
        let to = funded[(h as usize + 1) % funded.len()];
        node.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        let block = match node.produce_one() {
            Some(b) => realize(b),
            None => {
                eprintln!("producer returned None at height {h}; stopping early");
                break;
            }
        };
        let core = PrunedHeader::from(&block.header);
        full.put_block(h, &block).expect("put full");
        history
            .put_block(h, &HistoryBlock { core: core.clone(), transition: &block.transition, events: &block.events })
            .expect("put history");
        pruned.put_pruned(h, &core).expect("put pruned");
        json_chain.push(block);

        if h % step == 0 || h == n {
            full.compact();
            history.compact();
            pruned.compact();
            let json_bytes = serde_json::to_vec(&json_chain).unwrap().len() as u64;
            let json_rs = (json_bytes as f64 * 24.0 / 16.0) as u64; // RS K=16,PARITY=8
            let full_disk = full.disk_bytes();
            let history_disk = history.disk_bytes();
            let pruned_disk = pruned.disk_bytes();
            println!(
                "H={:>6}  json={:>9}  json+RS={:>9}  flux-full={:>9}  flux-history={:>9}  flux-pruned={:>9}",
                h, human(json_bytes as f64), human(json_rs as f64),
                human(full_disk as f64), human(history_disk as f64), human(pruned_disk as f64),
            );
            curve.push(serde_json::json!({
                "height": h,
                "json_bytes": json_bytes,
                "json_rs_bytes": json_rs,
                "fluxdb_full_bytes": full_disk,
                "fluxdb_history_bytes": history_disk,
                "fluxdb_pruned_bytes": pruned_disk,
            }));
        }
    }

    let produced = json_chain.len() as f64;
    let json_total = serde_json::to_vec(&json_chain).unwrap().len() as f64;
    let full_total = full.disk_bytes() as f64;
    let history_total = history.disk_bytes() as f64;
    let pruned_total = pruned.disk_bytes() as f64;
    let (json_pb, full_pb, history_pb, pruned_pb) =
        (json_total / produced, full_total / produced, history_total / produced, pruned_total / produced);

    // 100-year projection using the MEASURED on-disk per-block rates (real,
    // includes LSM overhead) — not the theoretical encoding size.
    let secs_100y = 100.0 * 365.25 * 24.0 * 3600.0;
    let cadences: [(&str, f64); 4] =
        [("100ms", 0.1), ("1s", 1.0), ("5s", 5.0), ("10s", 10.0)];
    let mut proj = Vec::new();
    println!("\n=== 100-year projection (MEASURED on-disk per-block rates) ===");
    println!(
        "per-block ON DISK: json={:.1}B  flux-full={:.1}B  flux-history={:.1}B  flux-pruned={:.1}B\n",
        json_pb, full_pb, history_pb, pruned_pb
    );
    println!("{:<8} {:>12} {:>12} {:>12} {:>12} {:>12}", "cadence", "blocks/100y", "JSON+RS", "flux-full", "flux-history", "flux-pruned");
    for (label, bs) in cadences {
        let blocks = secs_100y / bs;
        let j = blocks * json_pb * 24.0 / 16.0;
        let f = blocks * full_pb;
        let hi = blocks * history_pb;
        let p = blocks * pruned_pb;
        println!("{:<8} {:>12.2e} {:>12} {:>12} {:>12} {:>12}", label, blocks, human(j), human(f), human(hi), human(p));
        proj.push(serde_json::json!({
            "cadence": label, "block_seconds": bs, "blocks_100y": blocks,
            "json_rs_bytes": j, "fluxdb_full_bytes": f, "fluxdb_history_bytes": hi, "fluxdb_pruned_bytes": p,
        }));
    }

    let results = serde_json::json!({
        "generated_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "blocks_produced": produced as u64,
        "per_block_on_disk": { "json": json_pb, "fluxdb_full": full_pb, "fluxdb_history": history_pb, "fluxdb_pruned": pruned_pb },
        "shrink_vs_json": { "fluxdb_full": json_pb / full_pb, "fluxdb_history": json_pb / history_pb, "fluxdb_pruned": json_pb / pruned_pb },
        "tiers": {
            "full": "header+sigs+transition+events — re-verifiable, full tx history, bootstraps peers",
            "history": "drop crypto witness, keep transition+events — full tx history + state proofs, trusts past verify",
            "pruned": "roots+identity only — state + inclusion proofs, NO tx enumeration"
        },
        "curve": curve,
        "projection_100y": proj,
    });
    std::fs::write(&out, serde_json::to_vec_pretty(&results).unwrap()).expect("write results");
    println!("\n✓ results → {out}");
    let _ = std::fs::remove_dir_all(&base);
}
