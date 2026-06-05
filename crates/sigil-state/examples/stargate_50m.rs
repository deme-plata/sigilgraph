//! FLUXFOOD iteration toward 50M TPS — find the wall, move it, re-measure.
//!
//! Baseline (parallel_exec): ~5M TPS at 16 threads, BLAKE3 leaf, scalar
//! 256-bit add/sub, NUMA-plateaued. 50M is 10× past that. The levers, in
//! order of measured suspicion:
//!
//!   1. accumulator arithmetic — 32-byte scalar carry loop per add/sub,
//!      4× per tx. Vectorize to u64×4 limbs (V-LIMB).
//!   2. leaf hash cost — BLAKE3-of-80B dominates per-thread. Measure the
//!      sound BLAKE3 path AND the fast-leaf ceiling side by side, so the gap
//!      to 50M is quantified as exactly "the crypto lane."
//!   3. parallelism — sweep 1..48 threads, find the real plateau.
//!
//! Each row is one (leaf, threads) point; the harness prints the climb and
//! flags when a config clears 50M. Honest: the fast leaf is the UNSAFE
//! ceiling (non-crypto → state forgery in an additive accumulator); it
//! measures the headroom a vetted fast CRHF could reach, not a shippable hash.

use std::time::Instant;

const ACCOUNTS: u64 = 1_000_000;
const TXS: usize = 8_000_000;

#[derive(Clone, Copy)]
struct Tx { from: u64, to: u64 }
fn gen(n: usize) -> Vec<Tx> {
    let mut s=0x1234_5678_9abc_def0u64;
    let mut nx=||{s^=s<<13;s^=s>>7;s^=s<<17;s};
    (0..n).map(|_| Tx{from:nx()%ACCOUNTS, to:nx()%ACCOUNTS}).collect()
}

// ── leaf hashes ──────────────────────────────────────────────────────────────
#[inline] fn leaf_blake3(acct: u64, v: u128) -> [u64; 4] {
    let mut h = blake3::Hasher::new();
    h.update(b"L"); h.update(&acct.to_le_bytes()); h.update(&v.to_le_bytes());
    let b = *h.finalize().as_bytes();
    [u64::from_le_bytes(b[0..8].try_into().unwrap()),
     u64::from_le_bytes(b[8..16].try_into().unwrap()),
     u64::from_le_bytes(b[16..24].try_into().unwrap()),
     u64::from_le_bytes(b[24..32].try_into().unwrap())]
}
#[inline] fn mix(mut x:u64)->u64{ x^=x>>32; x=x.wrapping_mul(0xd6e8feb86659fd93); x^=x>>32; x }
#[inline] fn leaf_fast(acct: u64, v: u128) -> [u64; 4] {
    let base = mix(0xa0761d6478bd642f ^ acct ^ (v as u64) ^ ((v>>64) as u64));
    [mix(base), mix(base^0x9e3779b97f4a7c15), mix(base^0x3c6ef372fe94f82a), mix(base^0xdaa66d2b71a3c93f)]
}

// ── V-LIMB: u64×4 limb add/sub with carry (vs the 32-byte scalar loop) ───────
#[inline] fn add4(a:[u64;4], b:[u64;4]) -> [u64;4] {
    let (s0,c0)=a[0].overflowing_add(b[0]);
    let (s1,c1a)=a[1].overflowing_add(b[1]); let (s1,c1b)=s1.overflowing_add(c0 as u64);
    let (s2,c2a)=a[2].overflowing_add(b[2]); let (s2,c2b)=s2.overflowing_add((c1a|c1b) as u64);
    let s3=a[3].wrapping_add(b[3]).wrapping_add((c2a|c2b) as u64);
    [s0,s1,s2,s3]
}
#[inline] fn sub4(a:[u64;4], b:[u64;4]) -> [u64;4] {
    let (d0,br0)=a[0].overflowing_sub(b[0]);
    let (d1,br1a)=a[1].overflowing_sub(b[1]); let (d1,br1b)=d1.overflowing_sub(br0 as u64);
    let (d2,br2a)=a[2].overflowing_sub(b[2]); let (d2,br2b)=d2.overflowing_sub((br1a|br1b) as u64);
    let d3=a[3].wrapping_sub(b[3]).wrapping_sub((br2a|br2b) as u64);
    [d0,d1,d2,d3]
}

#[inline]
fn fold(txs:&[Tx], fast:bool) -> [u64;4] {
    let leaf = if fast { leaf_fast } else { leaf_blake3 };
    let mut s=[0u64;4];
    for tx in txs {
        s = add4(s, leaf(tx.from, 999_999));   // from new
        s = sub4(s, leaf(tx.from, 1_000_000));  // from old
        s = add4(s, leaf(tx.to, 1_000_001));    // to new
        s = sub4(s, leaf(tx.to, 1_000_000));    // to old
    }
    s
}

fn run(txs:&[Tx], nt:usize, fast:bool) -> ([u64;4], f64) {
    let chunk = txs.len().div_ceil(nt);
    let mut partials = vec![[0u64;4]; nt];
    let t0=Instant::now();
    std::thread::scope(|sc|{
        let mut hs=Vec::new();
        for (i,sl) in txs.chunks(chunk).enumerate(){ hs.push((i, sc.spawn(move|| fold(sl, fast)))); }
        for (i,h) in hs { partials[i]=h.join().unwrap(); }
    });
    let mut root=[0u64;4]; for p in &partials { root=add4(root,*p); }
    (root, TXS as f64 / t0.elapsed().as_secs_f64())
}

fn main(){
    let cores = std::thread::available_parallelism().map(|n|n.get()).unwrap_or(1);
    println!("\n  FLUXFOOD → 50M TPS  ({} txs · {} accounts · {} cores · u64-limb accumulator)\n", TXS, ACCOUNTS, cores);
    let txs = gen(TXS);
    // correctness: serial == parallel merge, both leaf kinds
    let ser_b = fold(&txs, false); let (par_b,_) = run(&txs, cores.min(16), false);
    assert_eq!(ser_b, par_b, "blake3 parallel merge != serial");
    let ser_f = fold(&txs, true);  let (par_f,_) = run(&txs, cores.min(16), true);
    assert_eq!(ser_f, par_f, "fast parallel merge != serial");
    println!("  ✓ commutative-merge correctness verified (parallel root == serial root)\n");

    for &fast in &[false, true] {
        let label = if fast {"fast leaf (UNSAFE ceiling)"} else {"BLAKE3 leaf (SOUND)"};
        println!("  ── {label} ──");
        println!("  ┌ threads ┬──────── TPS ─┬─ vs 50M ─┐");
        for &nt in &[1usize,2,4,8,16,24,32,48] {
            if nt>cores { continue; }
            let (_, tps) = run(&txs, nt, fast);
            let flag = if tps>=50_000_000.0 {"✓ 50M"} else if tps>=10_000_000.0 {"10M+"} else if tps>=1_000_000.0 {"1M+"} else {"—"};
            println!("  │ {nt:>7} │ {tps:>11.0} │ {flag:>7} │", );
        }
        println!("  └─────────┴──────────────┴─────────┘\n");
    }
}
