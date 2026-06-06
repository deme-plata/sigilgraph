//! Stargate #2 — parallel execution + the commutative-accumulator synergy.
//!
//! The #1b verdict: post-roots, execution is 99.9% of wall-clock, and the
//! accumulator's per-leaf BLAKE3 is ~half of THAT. Two facts make parallel
//! execution the dominant lever:
//!
//! 1. **SIGIL txs are mostly independent** (per-account). Non-conflicting
//!    txs apply across all 48 cores — Block-STM / Sui-style.
//! 2. **The additive accumulator merges commutatively.** Each worker thread
//!    keeps a partial 256-bit sum of its slice's leaf-hash deltas; at commit
//!    the partials add together (order-independent) into the same root a
//!    single thread would have produced. So **parallel execution also
//!    parallelizes the leaf hashing across cores for free** — this is the
//!    available form of "batched BLAKE3" (across cores, not SIMD lanes),
//!    with zero change to the hash and zero audit surface.
//!
//! This harness measures the dominant cost — leaf hashing + accumulation
//! for a block of txs — scaling across thread counts, and reports whether
//! it clears the Stargate target of 1,000,000 TPS. Balance-array writes are
//! cheaper than the hashing and shard the same way, so the hash-bound
//! measurement is the representative one (and the conservative one).
//!
//! No new deps — std::thread::scope. Each thread owns a disjoint tx slice
//! and produces a partial sum; no shared mutable state, no locks.

use std::time::Instant;

use sigil_state::Accumulator;

const WALLETS: u64 = 1_000_000;       // realistic large state
const TXS: usize = 4_000_000;         // one big block's worth of work

type Key = [u8; 32];
fn wkey(i: u64) -> Key { let mut k=[0u8;32]; k[..8].copy_from_slice(&i.to_le_bytes()); k }
fn val(a: u128) -> [u8;16] { a.to_le_bytes() }

#[derive(Clone, Copy)]
struct Tx { from: u64, to: u64, amt: u128 }

fn gen_txs(n: usize) -> Vec<Tx> {
    let mut s: u64 = 0x1234_5678_9abc_def0;
    let mut next = || { s^=s<<13; s^=s>>7; s^=s<<17; s };
    (0..n).map(|_| Tx { from: next()%WALLETS, to: next()%WALLETS, amt: 1 }).collect()
}

fn main() {
    println!("\n  STARGATE #2 — parallel execution scaling (Epsilon, {} cores)", num_cpus());
    println!("  {} txs · {} accounts · per-thread accumulator, commutative merge\n", TXS, WALLETS);

    let txs = gen_txs(TXS);

    // correctness anchor: serial fold of the full set must equal the
    // commutative merge of the parallel partials (proven below at nt run).
    let serial_root = fold_slice2(&txs);

    println!("  ┌ threads ┬──────── TPS ─┬─ speedup ─┬─ vs 1M ─┐");
    let mut base = 0.0f64;
    for &nt in &[1usize, 2, 4, 8, 16, 32, 48] {
        if nt > num_cpus() { continue; }
        // warm + measure
        let t0 = Instant::now();
        let partials = parallel_fold(&txs, nt);
        // commutative merge — order-independent
        let mut root = [0u8; 32];
        for p in &partials { root = add(root, *p); }
        let secs = t0.elapsed().as_secs_f64();
        let tps = TXS as f64 / secs;
        if nt == 1 { base = tps; }
        // correctness: parallel merge must equal the serial fold, every nt.
        assert_eq!(root, serial_root, "parallel merge diverged from serial at nt={nt}");
        let clears = if tps >= 1_000_000.0 { "✓ CLEARS" } else { "—" };
        println!("  │ {nt:>7} │ {tps:>11.0} │ {:>8.1}× │ {clears:>7} │", tps/base);
    }
    println!("  └─────────┴──────────────┴───────────┴─────────┘");
    println!("\n  Each thread folds a disjoint tx slice into a partial 256-bit sum;");
    println!("  partials merge commutatively into the identical root a single thread");
    println!("  would produce. Parallel execution thus parallelizes the leaf hashing");
    println!("  across cores — the sound, zero-audit form of \"batched BLAKE3\".\n");
    println!("  NOTE: this measures the hash-bound cost (the #1b bottleneck). Balance");
    println!("  writes are cheaper + shard identically, so real apply_tx scales at");
    println!("  least this well. Cross-shard credit routing adds an Amdahl term not");
    println!("  modeled here — chronos's full DAG harness measures that next.\n");
}

/// Split txs across `nt` threads, each producing a partial sum. std scoped
/// threads, no shared mutable state.
fn parallel_fold(txs: &[Tx], nt: usize) -> Vec<[u8; 32]> {
    let chunk = txs.len().div_ceil(nt);
    let mut partials = vec![[0u8; 32]; nt];
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for (i, slice) in txs.chunks(chunk).enumerate() {
            handles.push((i, s.spawn(move || fold_slice2(slice))));
        }
        for (i, h) in handles { partials[i] = h.join().unwrap(); }
    });
    partials
}

/// Correct per-leg fold: sum += new_from - old_from + new_to - old_to.
fn fold_slice2(txs: &[Tx]) -> [u8; 32] {
    let mut sum = [0u8; 32];
    for tx in txs {
        let fk = wkey(tx.from); let tk = wkey(tx.to);
        sum = add(sum, Accumulator::leaf_hash(&fk, &val(1_000_000 - tx.amt)));
        sum = sub(sum, Accumulator::leaf_hash(&fk, &val(1_000_000)));
        sum = add(sum, Accumulator::leaf_hash(&tk, &val(1_000_000 + tx.amt)));
        sum = sub(sum, Accumulator::leaf_hash(&tk, &val(1_000_000)));
    }
    sum
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[inline] fn add(a:[u8;32],b:[u8;32])->[u8;32]{ let mut o=[0u8;32]; let mut c=0u16; for i in 0..32{ let s=a[i] as u16+b[i] as u16+c; o[i]=s as u8; c=s>>8;} o }
#[inline] fn sub(a:[u8;32],b:[u8;32])->[u8;32]{ let mut o=[0u8;32]; let mut br=0i16; for i in 0..32{ let d=a[i] as i16-b[i] as i16-br; if d<0{o[i]=(d+256) as u8; br=1;}else{o[i]=d as u8; br=0;}} o }
