//! Serve-side throughput measurement for V7-SUPPLY. Run release:
//!   cargo run --release -p sigil-serve --example serve_bench
//! Synthetic in-RAM chain (no live node). Reports:
//!   (A) skeleton-page production rate (records/s) — the ≥100k blk/s serve feed
//!   (B) codec=4 trailer build: stateless O(chain) vs cached O(tail) per request

use sigil_header::SkeletonRecord;
use sigil_serve::{archive_root_range, skeleton_page, ArchiveRootCache, BlockSkeletonSource};
use std::time::Instant;

struct MemChain {
    recs: Vec<SkeletonRecord>,
}
impl MemChain {
    fn new(n: u64) -> Self {
        let mut recs = Vec::with_capacity(n as usize);
        let mut parent = [0u8; 32];
        for h in 0..n {
            let block_hash = *blake3::hash(&h.to_le_bytes()).as_bytes();
            recs.push(SkeletonRecord { height: h, block_hash, parent_hash: parent });
            parent = block_hash;
        }
        Self { recs }
    }
}
impl BlockSkeletonSource for MemChain {
    fn skeleton_at(&self, height: u64) -> Option<SkeletonRecord> {
        self.recs.get(height as usize).cloned()
    }
    fn tip(&self) -> u64 {
        self.recs.len().saturating_sub(1) as u64
    }
    fn skeleton_range(&self, from: u64, to: u64) -> Vec<SkeletonRecord> {
        let lo = from as usize;
        let hi = (to as usize + 1).min(self.recs.len());
        self.recs[lo..hi].to_vec()
    }
}

fn main() {
    let n: u64 = std::env::var("BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(500_000);
    let page: u64 = 50_000;
    eprintln!("building synthetic chain of {n} records…");
    let chain = MemChain::new(n);
    let tip = chain.tip();

    // (A) skeleton-page production rate (serialize 'S' pages across the chain)
    let t = Instant::now();
    let mut bytes = 0usize;
    let mut from = 0u64;
    while from <= tip {
        let to = (from + page - 1).min(tip);
        let pg = skeleton_page(&chain, from, to);
        bytes += pg.len();
        from = to + 1;
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "(A) skeleton-page production: {:.0} records/s  ({:.1} MB/s, {} bytes total)",
        n as f64 / secs,
        bytes as f64 / secs / 1.0e6,
        bytes
    );

    // (B1) stateless trailer = full O(chain) rehash from genesis
    let t = Instant::now();
    let r0 = archive_root_range(&chain, 0, tip);
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "(B1) stateless trailer (O(chain) rehash): {:.3} ms  ({:.0} records/s hashed)",
        secs * 1e3,
        n as f64 / secs
    );

    // (B2) cached trailer: warm once, then per-request cost re-finalizing at the
    // current anchor (the live-serve case: many clients, finalized prefix reused)
    let mut cache = ArchiveRootCache::with_interval(page);
    let t = Instant::now();
    let r1 = cache.root_prefix(&chain, tip); // warm-up (one-time O(chain))
    let warm = t.elapsed().as_secs_f64();
    assert_eq!(r0, r1, "cache must equal stateless root");

    // steady-state: repeated finalize at the same finalized anchor → O(tail)
    let iters = 200;
    let t = Instant::now();
    for _ in 0..iters {
        let _ = cache.root_prefix(&chain, tip);
    }
    let per = t.elapsed().as_secs_f64() / iters as f64;
    // warm-up IS one stateless-equivalent full hash → use it as the O(chain) baseline
    let speedup = warm / per.max(1e-9);
    eprintln!(
        "(B2) cached trailer: warm-up {:.3} ms (one-time), then {:.4} ms/request  ({} checkpoints, {:.0}x faster than stateless)",
        warm * 1e3,
        per * 1e3,
        cache.checkpoint_count(),
        speedup
    );
    eprintln!("roots match: {}", r0 == r1);
}
