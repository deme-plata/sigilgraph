//! mempool_bench — Phase B (SIGIL_BRAIDPOOL_v1_1.md §17): a real, measured
//! head-to-head between `sigil_tx::Mempool` (today's single global mutex) and
//! `sigil_narwhal_mempool::ShardedMempool` (Phase A) under equivalent
//! synthetic load.
//!
//! Deliberately NOT wired into `sigil-node`'s producer loop. Investigating the
//! wiring surfaced a real correctness hazard first: `sigil_api::AppState` and
//! `main.rs`'s block-body `pull()` share ONE `Arc<Mutex<Mempool>>` today — if
//! only the TXGEN/pull call site swapped backends behind a flag, real user
//! transactions submitted via `/v1/transactions` would go into the OLD
//! mempool and never get pulled into a block: silently dropped, not just
//! slow. A safe live swap needs `sigil_api` to also become backend-aware,
//! which is out of scope for this pass (bigger blast radius, another crate,
//! on a node whose producer loop was JUST stabilized this session — see
//! SIGIL_BRAIDPOOL_v1_1.md's own Phase-A discipline). This binary instead
//! delivers what Phase B is actually specified to deliver: "benchmark
//! against old global mutex" — a real number, not a live cutover.
//!
//! Two scenarios, both realistic (not cherry-picked for either mempool):
//! - SEQUENTIAL: one thread, one `ingest()` call per tx — the worst case for
//!   sharding (no lock contention to relieve; only batch-verify overhead
//!   from calling ingest() with a single-tx Vec each time, same for both).
//! - PARALLEL: N threads submitting concurrently, each its own share of txs
//!   — this is where sharding's actual benefit (independent lock domains)
//!   should show up, if it's real.
//!
//! Usage: mempool_bench [tx_count] [thread_count] [worker_count]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use sigil_narwhal_mempool::ShardedMempool;
use sigil_tx::{ed25519_keygen, ed25519_sign_tx, Mempool as LegacyMempool, SigilTx, SignedTx};

fn make_txs(n: usize) -> Vec<SignedTx> {
    (0..n)
        .map(|i| {
            let (sk, pk, wallet) = ed25519_keygen();
            let tx = SigilTx::Send { from: wallet, to: [1u8; 32], amount: i as u128, token: [0u8; 32], fee: 0 };
            ed25519_sign_tx(tx, &sk, &pk)
        })
        .collect()
}

fn split(txs: Vec<SignedTx>, parts: usize) -> Vec<Vec<SignedTx>> {
    let mut out: Vec<Vec<SignedTx>> = (0..parts).map(|_| Vec::new()).collect();
    for (i, tx) in txs.into_iter().enumerate() {
        out[i % parts].push(tx);
    }
    out
}

fn bench_legacy_sequential(txs: &[SignedTx]) -> (f64, usize) {
    let mp = LegacyMempool::new();
    let mp = Mutex::new(mp);
    let t0 = Instant::now();
    for tx in txs {
        mp.lock().unwrap().ingest(vec![tx.clone()]);
    }
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);
    let landed = mp.lock().unwrap().len();
    (landed as f64 / elapsed, landed)
}

fn bench_sharded_sequential(txs: &[SignedTx], workers: u16) -> (f64, usize) {
    let mp = ShardedMempool::new(workers, [0u8; 32]);
    let t0 = Instant::now();
    for tx in txs {
        mp.ingest(vec![tx.clone()]);
    }
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);
    let landed = mp.total_len();
    (landed as f64 / elapsed, landed)
}

fn bench_legacy_parallel(txs: Vec<SignedTx>, threads: usize) -> (f64, usize) {
    let mp = Arc::new(Mutex::new(LegacyMempool::new()));
    let chunks = split(txs, threads);
    let t0 = Instant::now();
    let handles: Vec<_> = chunks
        .into_iter()
        .map(|chunk| {
            let mp = Arc::clone(&mp);
            std::thread::spawn(move || {
                for tx in chunk {
                    mp.lock().unwrap().ingest(vec![tx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);
    let landed = mp.lock().unwrap().len();
    (landed as f64 / elapsed, landed)
}

fn bench_sharded_parallel(txs: Vec<SignedTx>, threads: usize, workers: u16) -> (f64, usize) {
    let mp = Arc::new(ShardedMempool::new(workers, [0u8; 32]));
    let chunks = split(txs, threads);
    let t0 = Instant::now();
    let handles: Vec<_> = chunks
        .into_iter()
        .map(|chunk| {
            let mp = Arc::clone(&mp);
            std::thread::spawn(move || {
                for tx in chunk {
                    mp.ingest(vec![tx]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);
    let landed = mp.total_len();
    (landed as f64 / elapsed, landed)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tx_count: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let threads: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    });
    let workers: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(threads as u16);

    println!("mempool_bench: tx_count={tx_count} threads={threads} sharded_workers={workers}");
    println!("(all txs use DISTINCT wallets — this is the realistic case for sharding's benefit;");
    println!(" repeated-wallet load would concentrate on one shard regardless of worker count.)\n");

    println!("── generating {tx_count} real ed25519-signed txs (this dominates wall time — excluded from the measured window) ──");
    let gen_t0 = Instant::now();
    let txs_seq = make_txs(tx_count);
    let txs_par_legacy = make_txs(tx_count);
    let txs_par_sharded = make_txs(tx_count);
    println!("  generation took {:.2}s\n", gen_t0.elapsed().as_secs_f64());

    println!("── SEQUENTIAL (1 thread, 1 tx per ingest() call) ──");
    let (legacy_seq_tps, legacy_seq_landed) = bench_legacy_sequential(&txs_seq);
    let (sharded_seq_tps, sharded_seq_landed) = bench_sharded_sequential(&txs_seq, workers);
    println!("  legacy  (1 mutex):      {legacy_seq_tps:>10.0} tx/s  ({legacy_seq_landed} landed)");
    println!("  sharded ({workers} workers):    {sharded_seq_tps:>10.0} tx/s  ({sharded_seq_landed} landed)");
    println!("  ratio: {:.2}x\n", sharded_seq_tps / legacy_seq_tps.max(1.0));

    println!("── PARALLEL ({threads} threads submitting concurrently) ──");
    let (legacy_par_tps, legacy_par_landed) = bench_legacy_parallel(txs_par_legacy, threads);
    let (sharded_par_tps, sharded_par_landed) = bench_sharded_parallel(txs_par_sharded, threads, workers);
    println!("  legacy  (1 mutex):      {legacy_par_tps:>10.0} tx/s  ({legacy_par_landed} landed)");
    println!("  sharded ({workers} workers):    {sharded_par_tps:>10.0} tx/s  ({sharded_par_landed} landed)");
    println!("  ratio: {:.2}x\n", sharded_par_tps / legacy_par_tps.max(1.0));

    // Sanity: every tx must actually land somewhere — a bench that silently
    // drops txs and reports a fast number is worse than no bench at all.
    let expected = tx_count;
    let all_landed = legacy_seq_landed == expected
        && sharded_seq_landed == expected
        && legacy_par_landed == expected
        && sharded_par_landed == expected;
    if !all_landed {
        eprintln!(
            "⚠ MISMATCH: expected {expected} landed txs in every scenario, got legacy_seq={legacy_seq_landed} sharded_seq={sharded_seq_landed} legacy_par={legacy_par_landed} sharded_par={sharded_par_landed}"
        );
        std::process::exit(1);
    }
    println!("✓ all {expected} txs landed in every scenario (no silent drops)");
}
