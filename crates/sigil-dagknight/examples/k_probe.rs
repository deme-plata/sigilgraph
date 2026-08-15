//! `k_probe` — single-cell forensic for the supercritical anomaly found by
//! `k_phase`: with k well below the critical line k_c = P-1, the node fed a
//! SHUFFLED arrival order ends up ordering far fewer blocks than the node fed
//! creation order (e.g. P=6 k=1: 421 vs 2400), and its common prefix then
//! disagrees.
//!
//! This binary tallies EVERY insert outcome on both nodes plus the full
//! `BraidStats`, so the missing blocks can be attributed to a specific
//! mechanism (parked-forever / below-final pruning / window overflow /
//! structural reject) instead of guessed at.
//!
//! Env: KPROBE_P (6), KPROBE_K (1), KPROBE_D (0), KPROBE_ROUNDS (400), KPROBE_SEED (42)

use std::collections::{HashMap, VecDeque};

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;

const GENESIS: BlockHash = [0u8; 32];

struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn block_hash(producer: usize, height: u64) -> BlockHash {
    let mut h = blake3::Hasher::new();
    h.update(b"kphase/blk");
    h.update(&(producer as u64).to_le_bytes());
    h.update(&height.to_le_bytes());
    *h.finalize().as_bytes()
}

fn producer_id(producer: usize) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"kphase/prod");
    h.update(&(producer as u64).to_le_bytes());
    *h.finalize().as_bytes()
}

#[derive(Default, Debug)]
struct Tally {
    inserted: u64,
    duplicate: u64,
    missing: u64,
    below_final: u64,
    rejected: u64,
    reject_reasons: HashMap<&'static str, u64>,
}

impl Tally {
    fn record(&mut self, o: &InsertOutcome) {
        match o {
            InsertOutcome::Inserted { .. } => self.inserted += 1,
            InsertOutcome::Duplicate => self.duplicate += 1,
            InsertOutcome::MissingParents(_) => self.missing += 1,
            InsertOutcome::BelowFinal { .. } => self.below_final += 1,
            InsertOutcome::Rejected(r) => {
                self.rejected += 1;
                *self.reject_reasons.entry(r).or_default() += 1;
            }
        }
    }
}

fn env<T: std::str::FromStr>(key: &str, d: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(d)
}

fn main() {
    let producers: usize = env("KPROBE_P", 6);
    let k: usize = env("KPROBE_K", 1);
    let delay: usize = env("KPROBE_D", 0);
    let rounds: usize = env("KPROBE_ROUNDS", 400);
    let seed: u64 = env("KPROBE_SEED", 42);

    println!("k_probe: P={producers} k={k} D={delay} rounds={rounds} seed={seed} (k_c = P-1 = {})", producers - 1);

    // ── generate the DAG (identical generator to k_phase) ─────────────────
    let mut backlog: Vec<VecDeque<(usize, BlockHash)>> = vec![VecDeque::new(); producers];
    let mut views: Vec<BlockView> = Vec::new();
    for r in 1..=rounds {
        let mut minted = Vec::with_capacity(producers);
        for p in 0..producers {
            let height = r as u64;
            let hash = block_hash(p, height);
            let parent = if r == 1 { GENESIS } else { block_hash(p, height - 1) };
            let mut merge_parents = Vec::with_capacity(k);
            while merge_parents.len() < k {
                match backlog[p].front() {
                    Some(&(vis, h)) if vis <= r => {
                        backlog[p].pop_front();
                        if h != parent && !merge_parents.contains(&h) {
                            merge_parents.push(h);
                        }
                    }
                    _ => break,
                }
            }
            views.push(BlockView { hash, parent, merge_parents, height, producer: producer_id(p) });
            minted.push((p, hash));
        }
        for (origin, hash) in &minted {
            for p in 0..producers {
                if p != *origin {
                    backlog[p].push_back((r + delay, *hash));
                }
            }
        }
    }

    // How far back do merge edges reach? That is the quantity the finality
    // window has to cover.
    let heights: HashMap<BlockHash, u64> = views.iter().map(|v| (v.hash, v.height)).collect();
    let mut max_reach = 0u64;
    let mut sum_reach = 0u64;
    let mut n_edges = 0u64;
    for v in &views {
        for mp in &v.merge_parents {
            if let Some(&ph) = heights.get(mp) {
                let reach = v.height.saturating_sub(ph);
                max_reach = max_reach.max(reach);
                sum_reach += reach;
                n_edges += 1;
            }
        }
    }
    println!(
        "DAG: {} blocks · merge edges {} · merge-edge reach: max {} heights, mean {:.1}",
        views.len(),
        n_edges,
        max_reach,
        if n_edges > 0 { sum_reach as f64 / n_edges as f64 } else { 0.0 }
    );

    // KPROBE_FINAL_DEPTH overrides the historical hardcoded 64 (kept as the
    // default so old k_phase/k_probe results stay reproducible) — added
    // 2026-08-15 to verify the BraidConfig::default() final_depth bump
    // (64 -> 512) against this exact reproduction.
    let final_depth: u64 = env("KPROBE_FINAL_DEPTH", 64u64);
    // KPROBE_GHOSTDAG=1 turns on v2 coloring (using the same `k` this probe
    // already sweeps); KPROBE_FINAL_BLUE_DEPTH (unset by default = v1
    // height-offset finality even with coloring on) opts into the v2.1
    // blue-score finality rule under test — added 2026-08-15 to measure it
    // against the exact adversarial scenario that exposed the original bug.
    let use_ghostdag: bool = env("KPROBE_GHOSTDAG", 0u32) != 0;
    let final_blue_depth: Option<u64> = std::env::var("KPROBE_FINAL_BLUE_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    let cfg = BraidConfig {
        final_depth,
        max_window: 1 << 20,
        max_pending: 1 << 18,
        max_merge_parents: k.max(1),
        ghostdag_k: if use_ghostdag { Some(k as u32) } else { None },
        final_blue_depth,
    };
    println!(
        "cfg: final_depth={} max_window={} max_pending={} max_merge_parents={} ghostdag_k={:?} final_blue_depth={:?}",
        cfg.final_depth, cfg.max_window, cfg.max_pending, cfg.max_merge_parents, cfg.ghostdag_k, cfg.final_blue_depth
    );

    // ── node A: creation order ────────────────────────────────────────────
    let mut a = Braid::new_with_base(cfg.clone(), GENESIS, 0);
    let mut ta = Tally::default();
    for v in &views {
        let o = a.insert(v.clone());
        ta.record(&o);
    }

    // ── node B: shuffled order + exhaustive backfill ───────────────────────
    // Delivery model. KPROBE_REORDER=0 (default) is a FULL random permutation:
    // maximally adversarial, and NOT how gossip behaves — a block from round
    // 400 can arrive before a block from round 1. KPROBE_REORDER=N is a
    // bounded forward reorder of window N, which is the realistic model (and
    // the one the crate's own S5 live-topology gate uses, with N=8).
    let reorder: usize = env("KPROBE_REORDER", 0usize);
    let mut order: Vec<usize> = (0..views.len()).collect();
    let mut rng = XorShift64::new(seed ^ 0xD1CE_5EED_u64);
    if reorder == 0 {
        for i in (1..order.len()).rev() {
            let j = (rng.next() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
    } else {
        for i in 0..order.len() {
            let w = (order.len() - i).min(reorder) as u64;
            if w > 1 {
                let j = i + (rng.next() % w) as usize;
                order.swap(i, j);
            }
        }
    }
    println!(
        "delivery: {}",
        if reorder == 0 {
            "FULL random permutation (adversarial, not a gossip model)".to_string()
        } else {
            format!("bounded forward reorder, window {reorder} (realistic gossip)")
        }
    );
    let mut b = Braid::new_with_base(cfg, GENESIS, 0);
    let mut tb = Tally::default();
    for &i in &order {
        let o = b.insert(views[i].clone());
        tb.record(&o);
    }
    let first_pass = tb.inserted;

    let index: HashMap<BlockHash, usize> = views.iter().enumerate().map(|(i, v)| (v.hash, i)).collect();
    let mut passes = 0;
    loop {
        passes += 1;
        if passes > 64 {
            break;
        }
        let mut progressed = false;
        for h in b.missing_parents() {
            if let Some(&i) = index.get(&h) {
                let o = b.insert(views[i].clone());
                tb.record(&o);
                if matches!(o, InsertOutcome::Inserted { .. }) {
                    progressed = true;
                }
            }
        }
        for v in &views {
            let o = b.insert(v.clone());
            tb.record(&o);
            if matches!(o, InsertOutcome::Inserted { .. }) {
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let sa = a.stats();
    let sb = b.stats();
    let la = a.linearize();
    let lb = b.linearize();

    println!("\n── node A (creation order) ──");
    println!("  outcomes: {ta:?}");
    println!("  stats: {sa:?}");
    println!("  linearized: {}", la.len());

    println!("\n── node B (shuffled, {} backfill passes) ──", passes);
    println!("  first-pass inserted: {first_pass}");
    println!("  outcomes (incl. backfill re-offers): {tb:?}");
    println!("  stats: {sb:?}");
    println!("  linearized: {}", lb.len());
    println!("  still-missing parents: {}", b.missing_parents().len());

    // Which blocks did B never order, and what do they look like?
    let in_b: std::collections::HashSet<BlockHash> = lb.iter().copied().collect();
    let lost: Vec<&BlockView> = views.iter().filter(|v| !in_b.contains(&v.hash)).collect();
    println!("\n── blocks node A ordered but node B did not: {} ──", lost.len());
    if let (Some(first), Some(last)) = (lost.first(), lost.last()) {
        println!("  height range of lost blocks: {} .. {}", first.height, last.height);
        let contained: usize = lost.iter().filter(|v| b.contains(&v.hash)).count();
        println!("  of those, still resident in B's braid (contains=true): {contained}");
    }

    // DECISIVE CHECK: are the hashes `missing_parents()` still asks for
    // actually blocks that are ALREADY parked in B's own pending set? If so,
    // re-inserting them is a no-op (`insert` short-circuits on Duplicate) and
    // no backfill loop built on that worklist can ever converge.
    let still = b.missing_parents();
    let mut known_to_generator = 0usize;
    let mut outcome_duplicate = 0usize;
    let mut outcome_other = 0usize;
    for h in &still {
        if let Some(&i) = index.get(h) {
            known_to_generator += 1;
            match b.insert(views[i].clone()) {
                InsertOutcome::Duplicate => outcome_duplicate += 1,
                _ => outcome_other += 1,
            }
        }
    }
    println!(
        "\n── worklist forensic: {} hashes still requested · {} exist in the generated set · \
         re-insert outcome: Duplicate {} / other {} ──",
        still.len(),
        known_to_generator,
        outcome_duplicate,
        outcome_other
    );
    println!(
        "  (Duplicate == the requested 'missing parent' is itself already parked in B's pending set)"
    );

    let div = la.iter().zip(lb.iter()).filter(|(x, y)| x != y).count();
    println!("\ncommon-prefix divergence: {div} · order_hash match: {}", a.order_hash() == b.order_hash());
}
