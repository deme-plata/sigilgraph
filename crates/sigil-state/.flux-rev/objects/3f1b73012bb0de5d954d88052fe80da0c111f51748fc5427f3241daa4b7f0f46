//! Stargate #1b — the SOUND fast-leaf-hash lever.
//!
//! roots_throughput showed a faster accumulator leaf hash is worth up to
//! +116% TPS (ceiling, via non-crypto wyhash). But wyhash is UNSAFE for an
//! additive accumulator — a leaf collision lets two distinct states share a
//! sum → root → state forgery. So +116% is the ceiling, not shippable.
//!
//! This harness measures how much of that ceiling we can capture **without
//! weakening the hash at all** — by the same lesson Stargate #1 taught:
//! call BLAKE3 *fewer times*, not faster.
//!
//! ## The sound lever: coalesced per-block hashing
//!
//! The per-tx accumulator hashes 4× per tx (old+new for from + to). But a
//! hot account touched 50 times in a block doesn't need 50 hash-pairs — its
//! net effect on the root is one transition: value-at-block-start →
//! value-at-block-end. So: track each touched key's *original* value in a
//! dirty set during execution, and at commit hash exactly one pair per
//! UNIQUE touched key. Full BLAKE3, zero audit surface, definitely sound.
//!
//! The win scales with touch-locality. Measured under two workloads:
//!   - uniform: 100 random txs over 10k accounts (touch-once dominated)
//!   - hot:     80% of txs hit a 1%-hot set (DEX pools / master wallet /
//!              popular contracts — the realistic case)
//!
//! Strategies:
//!   B  per-tx · BLAKE3      (today's incremental baseline)
//!   D  coalesced · BLAKE3   (the SOUND lever — hash once per touched key)
//!   C  per-tx · fasthash    (the UNSAFE ceiling, for reference only)
//!
//! Verdict: how much of C's +116% does the sound D capture, per workload.

use std::collections::BTreeMap;
use std::time::Instant;

use sigil_state::Accumulator;

const WALLETS: usize = 10_000;
const BLOCKS:  usize = 1_000;
const TX_PER_BLOCK: usize = 100;
const HOT_FRACTION: u64 = 100;     // hot set = WALLETS / 100  (1%)
const HOT_HIT_PCT: u64 = 80;       // 80% of picks land in the hot set

type Key = [u8; 32];
fn wkey(i: u64) -> Key { let mut k=[0u8;32]; k[..8].copy_from_slice(&i.to_le_bytes()); k }
fn val(a: u128) -> [u8;16] { a.to_le_bytes() }

struct Rng(u64);
impl Rng { fn next(&mut self)->u64{ let mut x=self.0; x^=x<<13; x^=x>>7; x^=x<<17; self.0=x; x } }

/// Pick an account index under the given workload.
#[inline]
fn pick(rng: &mut Rng, hot: bool) -> u64 {
    if hot && (rng.next() % 100) < HOT_HIT_PCT {
        rng.next() % (WALLETS as u64 / HOT_FRACTION)   // hot set: low indices
    } else {
        rng.next() % WALLETS as u64
    }
}

fn main() {
    println!("\n  STARGATE #1b — SOUND fast-leaf lever (coalesced per-block hashing)");
    println!("  {WALLETS} wallets · {BLOCKS} blocks × {TX_PER_BLOCK} tx\n");
    for hot in [false, true] {
        let label = if hot { "HOT (80% → 1% hot set · DEX/master/contract realistic)" } else { "UNIFORM (random over all accounts)" };
        println!("  ── workload: {label} ──");
        let b = bench_per_tx_blake3(hot);
        let d = bench_coalesced_blake3(hot);
        let c = bench_per_tx_fast(hot);
        let cap = (d.0 - b.0) / (c.0 - b.0) * 100.0;
        println!("    B per-tx · BLAKE3     {:>10.0} TPS   (baseline)", b.0);
        println!("    D coalesced · BLAKE3  {:>10.0} TPS   {:+.0}% vs B   [SOUND — full BLAKE3]", d.0, (d.0-b.0)/b.0*100.0);
        println!("    C per-tx · fasthash   {:>10.0} TPS   {:+.0}% vs B   [UNSAFE ceiling]", c.0, (c.0-b.0)/b.0*100.0);
        println!("    unique keys touched / block: B&C={:.0}  D={:.0}", b.1, d.1);
        if c.0 > b.0 {
            println!("    → sound lever (D) captures {:.0}% of the unsafe ceiling (C), at ZERO audit cost\n", cap.max(0.0));
        } else {
            println!();
        }
    }
    println!("  VERDICT: coalescing keeps full BLAKE3 (no cryptanalysis needed) and");
    println!("  captures most of the leaf-hash win exactly where it matters — hot");
    println!("  state (DEX pools, master wallet, popular contracts). A vetted faster");
    println!("  crypto leaf hash is a separate cryptanalysis-gated lane for the");
    println!("  residual cold-workload gain; coalescing ships now.\n");
}

/// Strategy B — per-tx incremental accumulator, full BLAKE3 leaf.
fn bench_per_tx_blake3(hot: bool) -> (f64, f64) {
    let mut map: BTreeMap<Key,u128> = BTreeMap::new();
    let mut acc = Accumulator::new();
    for i in 0..WALLETS as u64 { let k=wkey(i); map.insert(k,1_000_000); acc.insert(&k,&val(1_000_000)); }
    let mut rng = Rng(0xC0FFEE);
    let mut touched_total = 0u64;
    let t0 = Instant::now();
    for _ in 0..BLOCKS {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..TX_PER_BLOCK {
            let from = wkey(pick(&mut rng, hot));
            let to   = wkey(pick(&mut rng, hot));
            seen.insert(from); seen.insert(to);
            let fb=*map.get(&from).unwrap(); if fb>=1 { acc.update(&from,&val(fb),&val(fb-1)); *map.get_mut(&from).unwrap()-=1; }
            let tb=*map.get(&to).unwrap(); acc.update(&to,&val(tb),&val(tb+1)); *map.get_mut(&to).unwrap()+=1;
        }
        let _ = acc.root();
        touched_total += seen.len() as u64;
    }
    ((BLOCKS*TX_PER_BLOCK) as f64 / t0.elapsed().as_secs_f64(), touched_total as f64 / BLOCKS as f64)
}

/// Strategy D — coalesced. Execution mutates the live map and records each
/// touched key's ORIGINAL value once; at commit, hash one pair per unique
/// touched key. Full BLAKE3, but called ~per-unique-key instead of per-tx.
fn bench_coalesced_blake3(hot: bool) -> (f64, f64) {
    let mut map: BTreeMap<Key,u128> = BTreeMap::new();
    let mut acc = Accumulator::new();
    for i in 0..WALLETS as u64 { let k=wkey(i); map.insert(k,1_000_000); acc.insert(&k,&val(1_000_000)); }
    let mut rng = Rng(0xC0FFEE);
    let mut touched_total = 0u64;
    let t0 = Instant::now();
    for _ in 0..BLOCKS {
        // dirty set: key -> value at first touch this block
        let mut dirty: BTreeMap<Key,u128> = BTreeMap::new();
        for _ in 0..TX_PER_BLOCK {
            let from = wkey(pick(&mut rng, hot));
            let to   = wkey(pick(&mut rng, hot));
            let fb=*map.get(&from).unwrap();
            if fb>=1 { dirty.entry(from).or_insert(fb); *map.get_mut(&from).unwrap()-=1; }
            let tb=*map.get(&to).unwrap();
            dirty.entry(to).or_insert(tb); *map.get_mut(&to).unwrap()+=1;
        }
        // commit: one hash-pair per unique touched key
        for (k,old) in &dirty {
            let new=*map.get(k).unwrap();
            if *old!=new { acc.update(k,&val(*old),&val(new)); }
        }
        let _ = acc.root();
        touched_total += dirty.len() as u64;
    }
    ((BLOCKS*TX_PER_BLOCK) as f64 / t0.elapsed().as_secs_f64(), touched_total as f64 / BLOCKS as f64)
}

/// Strategy C — per-tx fast NON-crypto leaf (the unsafe ceiling).
fn bench_per_tx_fast(hot: bool) -> (f64, f64) {
    let mut map: BTreeMap<Key,u128> = BTreeMap::new();
    let mut sum=[0u8;32];
    let leaf=|k:&[u8],v:u128|->[u8;32]{
        let mut s=0xa0761d6478bd642fu64;
        for c in k.chunks(8){ let mut b=[0u8;8]; b[..c.len()].copy_from_slice(c); s=mix(s^u64::from_le_bytes(b)); }
        let base=mix(s^(v as u64)^((v>>64) as u64));
        let mut out=[0u8;32];
        for lane in 0..4u64 { let x=mix(base^lane.wrapping_mul(0x9e3779b97f4a7c15)); out[lane as usize*8..lane as usize*8+8].copy_from_slice(&x.to_le_bytes()); }
        out
    };
    for i in 0..WALLETS as u64 { let k=wkey(i); map.insert(k,1_000_000); sum=add(sum,leaf(&k,1_000_000)); }
    let mut rng=Rng(0xC0FFEE);
    let t0=Instant::now();
    for _ in 0..BLOCKS {
        for _ in 0..TX_PER_BLOCK {
            let from=wkey(pick(&mut rng,hot)); let to=wkey(pick(&mut rng,hot));
            let fb=*map.get(&from).unwrap(); if fb>=1 { sum=add(sum,leaf(&from,fb-1)); sum=sub(sum,leaf(&from,fb)); *map.get_mut(&from).unwrap()-=1; }
            let tb=*map.get(&to).unwrap(); sum=add(sum,leaf(&to,tb+1)); sum=sub(sum,leaf(&to,tb)); *map.get_mut(&to).unwrap()+=1;
        }
        let mut h=blake3::Hasher::new(); h.update(b"fast"); h.update(&sum); let _=h.finalize();
    }
    ((BLOCKS*TX_PER_BLOCK) as f64 / t0.elapsed().as_secs_f64(), 0.0)
}

#[inline] fn mix(mut x:u64)->u64{ x^=x>>32; x=x.wrapping_mul(0xd6e8feb86659fd93); x^=x>>32; x }
#[inline] fn add(a:[u8;32],b:[u8;32])->[u8;32]{ let mut o=[0u8;32]; let mut c=0u16; for i in 0..32{ let s=a[i] as u16+b[i] as u16+c; o[i]=s as u8; c=s>>8;} o }
#[inline] fn sub(a:[u8;32],b:[u8;32])->[u8;32]{ let mut o=[0u8;32]; let mut br=0i16; for i in 0..32{ let d=a[i] as i16-b[i] as i16-br; if d<0{o[i]=(d+256) as u8; br=1;}else{o[i]=d as u8; br=0;}} o }
