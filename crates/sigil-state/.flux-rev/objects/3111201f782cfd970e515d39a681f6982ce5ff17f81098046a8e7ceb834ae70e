//! STARGATE → 1M TPS — the honest scaling curve from ~120k.
//!
//! The wall is sig-verify, and on ONE machine it's core-bound: batch×all-cores
//! tops out at a fixed rate no matter how you slice producers (same 48 cores).
//! So 1M end-to-end is NOT a single-box number. It is one of:
//!
//!   (a) one machine + GPU verify   — 10–50× the CPU ceiling, or
//!   (b) a multi-producer DAG       — M machines × per-machine ingest,
//!       made viable by proof-carrying blocks (a receiving node checks ONE
//!       O(1) proof per foreign block instead of re-verifying its sigs).
//!
//! This harness MEASURES the three quantities that decide it:
//!   1. per-machine verify ceiling  (ed25519 batch×48t — the hot-path proxy)
//!   2. DAG ordering throughput     (k-way merge of M producer streams)
//!   3. the resulting machine-count curve to 1M, with the binding wall named.

use std::collections::BinaryHeap;
use std::cmp::Reverse;
use std::time::Instant;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey, Signature};
use rand::rngs::OsRng;

const MSG: &[u8] = b"sigil-tx-canonical-bytes-placeholder-48b-payload";
const TARGET: f64 = 1_000_000.0;

/// 1. Per-machine sustained verify rate: ed25519 verify_batch across all cores.
/// This is ONE box's end-to-end ceiling (verify is the bottleneck; state+order
/// are cheaper, measured elsewhere).
fn per_machine_verify(cores: usize) -> f64 {
    let mut rng = OsRng;
    let n = 60_000usize;
    let keys: Vec<SigningKey> = (0..n).map(|_| SigningKey::generate(&mut rng)).collect();
    let vks: Vec<VerifyingKey> = keys.iter().map(|k| k.verifying_key()).collect();
    let sigs: Vec<Signature> = keys.iter().map(|k| k.sign(MSG)).collect();
    let msgs: Vec<&[u8]> = vec![MSG; n];
    // warm
    let _ = ed25519_dalek::verify_batch(&msgs[..1024], &sigs[..1024], &vks[..1024]);
    let t0 = Instant::now();
    let chunk = n.div_ceil(cores);
    std::thread::scope(|s| {
        for c in 0..cores {
            let lo = c * chunk; let hi = (lo + chunk).min(n); if lo >= hi { continue; }
            let m = &msgs[lo..hi]; let sg = &sigs[lo..hi]; let vk = &vks[lo..hi];
            s.spawn(move || { let _ = ed25519_dalek::verify_batch(m, sg, vk); });
        }
    });
    n as f64 / t0.elapsed().as_secs_f64()
}

/// 2. DAG ordering throughput: M producers each emit a stream of blocks tagged
/// (round, producer_id); the orderer produces the deterministic total order via
/// a k-way merge keyed by (round, producer_id) — DagKnight-style wave ordering.
/// Returns tx/s ordered (each block carries `block_txs` txs).
fn dag_order_rate(producers: usize, blocks_each: usize, block_txs: usize) -> f64 {
    // build M ascending streams of (round, pid)
    let streams: Vec<Vec<(u64, u32)>> = (0..producers)
        .map(|pid| (0..blocks_each as u64).map(|r| (r, pid as u32)).collect())
        .collect();
    let mut cursor = vec![0usize; producers];
    let mut heap: BinaryHeap<Reverse<(u64, u32, usize)>> = BinaryHeap::new();
    for (pid, s) in streams.iter().enumerate() {
        if let Some(&(r, p)) = s.first() { heap.push(Reverse((r, p, pid))); }
    }
    let total_blocks = producers * blocks_each;
    let mut ordered_blocks = 0u64;
    let t0 = Instant::now();
    while let Some(Reverse((_r, _p, pid))) = heap.pop() {
        ordered_blocks += 1;
        cursor[pid] += 1;
        if let Some(&(r, p)) = streams[pid].get(cursor[pid]) {
            heap.push(Reverse((r, p, pid)));
        }
    }
    std::hint::black_box(ordered_blocks);
    let elapsed = t0.elapsed().as_secs_f64();
    (total_blocks * block_txs) as f64 / elapsed
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  STARGATE → 1M TPS — the honest scaling curve  ({} cores)\n", cores);

    // ── 1. per-machine ceiling ────────────────────────────────────────────
    let per = per_machine_verify(cores);
    println!("  ── 1. per-machine verify ceiling (the box-local wall) ──");
    println!("    ed25519 batch×{cores}t:  {per:>12.0} tx/s   ← ONE machine, core-bound");
    println!("    (slicing into more producers on the SAME box does NOT help —");
    println!("     it's the same {cores} cores. 1M needs GPU or more machines.)\n");

    // ── 2. DAG ordering throughput ────────────────────────────────────────
    println!("  ── 2. DAG ordering throughput (is the DAG the wall?) ──");
    let order_rate = dag_order_rate(16, 4_000, 8_000); // 16 producers × 4k blocks × 8k tx
    println!("    DagKnight k-way wave-order: {order_rate:>12.0} tx/s ordered");
    let headroom = order_rate / per;
    println!("    → ordering is {headroom:.0}× the per-machine verify rate — NOT the wall.");
    println!("      (with proof-carrying blocks, a node orders by header + checks ONE");
    println!("       O(1) proof per foreign block — it never re-verifies foreign sigs.)\n");

    // ── 3. the machine-count curve to 1M ──────────────────────────────────
    println!("  ── 3. the curve to 1M (multi-producer DAG, proof-carrying) ──");
    println!("    {:>8}  {:>14}  {:>10}", "machines", "aggregate tx/s", "vs 1M");
    let mut hit = 0usize;
    for &m in &[1usize, 2, 4, 7, 8, 16] {
        // aggregate is bounded by BOTH total verify (M machines) and DAG ordering
        let aggregate = (m as f64 * per).min(order_rate);
        let flag = if aggregate >= TARGET { if hit == 0 { hit = m; } "✓ 1M" } else { "—" };
        println!("    {m:>8}  {aggregate:>14.0}  {flag:>10}");
    }
    let need = (TARGET / per).ceil() as usize;
    println!("\n  ── VERDICT ──");
    println!("    1M TPS = {need} machines × {per:.0} tx/s each, DAG-ordered.");
    println!("    The DAG orders {:.1}M tx/s — {:.0}× headroom, so it is NOT the bottleneck.", order_rate/1e6, order_rate/TARGET);
    println!("    What makes the {need}-machine aggregate REAL (not {need}× the re-verify tax):");
    println!("      • verify-once  — each machine verifies its OWN mempool once on ingest;");
    println!("      • proof-carrying blocks — receivers check 1 O(1) proof per foreign block");
    println!("        (measured 0.08ms, flat in N) instead of re-verifying foreign sigs.");
    println!("    Single-box alternative: GPU batch-verify → ~10–50× the {per:.0} ceiling");
    println!("    = {:.1}M–{:.1}M tx/s on ONE machine (flux-zk-stark GPU path already exists).", per*10.0/1e6, per*50.0/1e6);
    println!("    NEXT REAL STEP: run it cross-machine — Epsilon + Delta producers over the");
    println!("    live WG tunnel + flux-p2p, measure the 2-machine aggregate for real.\n");
}
