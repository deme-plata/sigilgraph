//! Roots-throughput wind tunnel — lets chronos judge the "BLAKE4" lever.
//!
//! Reproduces the chronos finding (roots = ~73% of wall-clock on small
//! blocks under the whole-map rehash) end-to-end through a transfer
//! workload that mirrors `apply_tx` + `commit_state_transition`, then
//! measures TPS under three root strategies:
//!
//!   A. whole-map rehash       — today's baseline (O(n) per block)
//!   B. accumulator · BLAKE3   — Stargate #1 incremental root (O(1)/leaf)
//!   C. accumulator · fasthash — same, but a fast NON-crypto leaf hash
//!                               (the ceiling of any reduced-round "BLAKE4")
//!
//! The verdict rule (Viktor / the Stargate loop):
//!   if C ≈ B  → a faster internal hash buys ~nothing once roots are O(1);
//!               BLAKE4 is dead, don't spend audit surface on it.
//!   if C ≫ B  → leaf hashing is a real residual cost; a reduced-round
//!               internal hash behind the crypto-agility flag is a real lane.
//!
//! Run:  fluxc build --package sigil-state --example roots_throughput --release
//!       then execute the produced binary.

use std::collections::BTreeMap;
use std::time::Instant;

use sigil_state::Accumulator;

// ── workload ────────────────────────────────────────────────────────────────
const WALLETS: usize = 10_000;   // pre-seeded accounts
const BLOCKS:  usize = 1_000;    // blocks per run
const TX_PER_BLOCK: usize = 100; // small blocks — the regime where roots hurt

type Key = [u8; 32];

fn wkey(i: u64) -> Key { let mut k = [0u8; 32]; k[..8].copy_from_slice(&i.to_le_bytes()); k }
fn val(a: u128) -> [u8; 16] { a.to_le_bytes() }

// Deterministic xorshift so every strategy sees the identical tx stream.
struct Rng(u64);
impl Rng { fn next(&mut self) -> u64 { let mut x=self.0; x^=x<<13; x^=x>>7; x^=x<<17; self.0=x; x } }

// ── strategy A: whole-map rehash (the O(n) baseline) ─────────────────────────
fn whole_map_root(map: &BTreeMap<Key, u128>) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for (k, v) in map { h.update(k); h.update(&v.to_le_bytes()); }
    *h.finalize().as_bytes()
}

// ── strategy C: fast NON-crypto leaf accumulator ─────────────────────────────
// A wyhash-style multiply-xor mix producing a 32-byte leaf. ~Fastest a leaf
// hash can plausibly be; the *upper bound* of what reduced-round BLAKE3 /
// xxh3 could ever buy. Additive-sum accumulator structure identical to the
// real one, so the only variable is the leaf-hash cost.
struct FastAcc { sum: [u8; 32], count: u64 }
impl FastAcc {
    fn new() -> Self { Self { sum: [0u8; 32], count: 0 } }
    #[inline] fn leaf(key: &[u8], v: u128) -> [u8; 32] {
        // 4 independent 64-bit wyhash lanes → 32 bytes.
        let mut out = [0u8; 32];
        let base = {
            let mut s = 0xa0761d6478bd642fu64;
            for chunk in key.chunks(8) {
                let mut b = [0u8; 8]; b[..chunk.len()].copy_from_slice(chunk);
                s = mix(s ^ u64::from_le_bytes(b));
            }
            mix(s ^ (v as u64) ^ ((v >> 64) as u64))
        };
        for lane in 0..4u64 {
            let x = mix(base ^ (lane.wrapping_mul(0x9e3779b97f4a7c15)));
            out[lane as usize*8..lane as usize*8+8].copy_from_slice(&x.to_le_bytes());
        }
        out
    }
    #[inline] fn update(&mut self, key: &[u8], old: u128, new: u128) {
        let o = Self::leaf(key, old); let n = Self::leaf(key, new);
        self.sum = add(self.sum, n); self.sum = sub(self.sum, o);
    }
    #[inline] fn insert(&mut self, key: &[u8], v: u128) { self.sum = add(self.sum, Self::leaf(key, v)); self.count += 1; }
    fn root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new(); // root stays BLAKE3 — only the leaf is fast
        h.update(b"fastacc-root"); h.update(&self.sum); h.update(&self.count.to_le_bytes());
        *h.finalize().as_bytes()
    }
}
#[inline] fn mix(mut x: u64) -> u64 { x ^= x >> 32; x = x.wrapping_mul(0xd6e8feb86659fd93); x ^= x >> 32; x }
#[inline] fn add(a: [u8;32], b: [u8;32]) -> [u8;32] { let mut o=[0u8;32]; let mut c=0u16; for i in 0..32 { let s=a[i] as u16+b[i] as u16+c; o[i]=s as u8; c=s>>8; } o }
#[inline] fn sub(a: [u8;32], b: [u8;32]) -> [u8;32] { let mut o=[0u8;32]; let mut br=0i16; for i in 0..32 { let d=a[i] as i16-b[i] as i16-br; if d<0 {o[i]=(d+256) as u8; br=1;} else {o[i]=d as u8; br=0;} } o }

fn main() {
    println!("\n  ROOTS-THROUGHPUT WIND TUNNEL");
    println!("  {WALLETS} wallets · {BLOCKS} blocks × {TX_PER_BLOCK} tx ({} tx total)\n",
        BLOCKS * TX_PER_BLOCK);

    // ── A: whole-map rehash ──────────────────────────────────────────────
    let (a_tps, a_exec, a_roots) = {
        let mut map: BTreeMap<Key, u128> = (0..WALLETS as u64).map(|i| (wkey(i), 1_000_000u128)).collect();
        let mut rng = Rng(0x1234_5678);
        let mut exec = std::time::Duration::ZERO; let mut roots = std::time::Duration::ZERO;
        let t0 = Instant::now();
        for _ in 0..BLOCKS {
            let te = Instant::now();
            for _ in 0..TX_PER_BLOCK {
                let from = wkey(rng.next() % WALLETS as u64);
                let to   = wkey(rng.next() % WALLETS as u64);
                let amt = 1u128;
                if let Some(b) = map.get_mut(&from) { if *b >= amt { *b -= amt; } }
                *map.get_mut(&to).unwrap() += amt;
            }
            exec += te.elapsed();
            let tr = Instant::now(); let _ = whole_map_root(&map); roots += tr.elapsed();
        }
        let total = t0.elapsed();
        ((BLOCKS*TX_PER_BLOCK) as f64 / total.as_secs_f64(), exec, roots)
    };

    // ── B: accumulator · full BLAKE3 leaf ────────────────────────────────
    let (b_tps, b_exec, b_roots) = run_acc_blake3();
    // ── C: accumulator · fast non-crypto leaf ────────────────────────────
    let (c_tps, c_exec, c_roots) = run_acc_fast();

    let pct = |r: std::time::Duration, e: std::time::Duration| 100.0*r.as_secs_f64()/(r+e).as_secs_f64();
    println!("  ┌─ strategy ───────────────────┬──────── TPS ─┬─ roots% ─┬─ exec% ─┐");
    println!("  │ A  whole-map rehash (today)  │ {a_tps:>11.0} │ {:>7.0}% │ {:>6.0}% │", pct(a_roots,a_exec), 100.0-pct(a_roots,a_exec));
    println!("  │ B  accumulator · BLAKE3      │ {b_tps:>11.0} │ {:>7.1}% │ {:>6.1}% │", pct(b_roots,b_exec), 100.0-pct(b_roots,b_exec));
    println!("  │ C  accumulator · fasthash    │ {c_tps:>11.0} │ {:>7.1}% │ {:>6.1}% │", pct(c_roots,c_exec), 100.0-pct(c_roots,c_exec));
    println!("  └──────────────────────────────┴──────────────┴──────────┴─────────┘");
    println!("\n  Stargate #1 (A→B): {:.0}× TPS", b_tps/a_tps);
    let lever = (c_tps - b_tps)/b_tps*100.0;
    println!("  \"BLAKE4\" lever (B→C, fastest possible leaf hash): {:+.1}% TPS", lever);
    if lever.abs() < 5.0 {
        println!("  → VERDICT: <5% — a faster internal hash is NOT worth the audit surface.");
        println!("            Roots are O(1); the residual cost is execution, not hashing.");
    } else {
        println!("  → VERDICT: ≥5% — reduced-round internal leaf hash is a real lane.");
        println!("            Prototype it behind the crypto-agility flag, keep root = BLAKE3.");
    }
    println!();
}

fn run_acc_blake3() -> (f64, std::time::Duration, std::time::Duration) {
    let mut map: BTreeMap<Key, u128> = BTreeMap::new();
    let mut acc = Accumulator::new();
    for i in 0..WALLETS as u64 { let k=wkey(i); map.insert(k, 1_000_000); acc.insert(&k, &val(1_000_000)); }
    let mut rng = Rng(0x1234_5678);
    let mut exec = std::time::Duration::ZERO; let mut roots = std::time::Duration::ZERO;
    let t0 = Instant::now();
    for _ in 0..BLOCKS {
        let te = Instant::now();
        for _ in 0..TX_PER_BLOCK {
            let from = wkey(rng.next() % WALLETS as u64);
            let to   = wkey(rng.next() % WALLETS as u64);
            let amt = 1u128;
            let fb = *map.get(&from).unwrap(); if fb>=amt { acc.update(&from,&val(fb),&val(fb-amt)); *map.get_mut(&from).unwrap()-=amt; }
            let tb = *map.get(&to).unwrap(); acc.update(&to,&val(tb),&val(tb+amt)); *map.get_mut(&to).unwrap()+=amt;
        }
        exec += te.elapsed();
        let tr = Instant::now(); let _ = acc.root(); roots += tr.elapsed();
    }
    ((BLOCKS*TX_PER_BLOCK) as f64 / t0.elapsed().as_secs_f64(), exec, roots)
}

fn run_acc_fast() -> (f64, std::time::Duration, std::time::Duration) {
    let mut map: BTreeMap<Key, u128> = BTreeMap::new();
    let mut acc = FastAcc::new();
    for i in 0..WALLETS as u64 { let k=wkey(i); map.insert(k, 1_000_000); acc.insert(&k, 1_000_000); }
    let mut rng = Rng(0x1234_5678);
    let mut exec = std::time::Duration::ZERO; let mut roots = std::time::Duration::ZERO;
    let t0 = Instant::now();
    for _ in 0..BLOCKS {
        let te = Instant::now();
        for _ in 0..TX_PER_BLOCK {
            let from = wkey(rng.next() % WALLETS as u64);
            let to   = wkey(rng.next() % WALLETS as u64);
            let amt = 1u128;
            let fb = *map.get(&from).unwrap(); if fb>=amt { acc.update(&from,fb,fb-amt); *map.get_mut(&from).unwrap()-=amt; }
            let tb = *map.get(&to).unwrap(); acc.update(&to,tb,tb+amt); *map.get_mut(&to).unwrap()+=amt;
        }
        exec += te.elapsed();
        let tr = Instant::now(); let _ = acc.root(); roots += tr.elapsed();
    }
    ((BLOCKS*TX_PER_BLOCK) as f64 / t0.elapsed().as_secs_f64(), exec, roots)
}
