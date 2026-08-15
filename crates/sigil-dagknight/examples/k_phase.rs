//! `k_phase` — a MEASURED phase-transition sweep over the braid's merge-capacity
//! parameter `k`, against producer concurrency `P` and gossip delay `D`.
//!
//! ## What `k` means here (honest framing — read before quoting any number)
//!
//! `sigil-dagknight` v1 implements DETERMINISTIC BRAID LINEARIZATION. It has
//! **no GHOSTDAG blue score and no k-cluster** (see the crate header and
//! `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §1). So the `k` swept here is NOT the
//! GHOSTDAG/DagKnight-paper anticone bound, and NOT the Quillon `k_parameter`
//! decentralization EMA. It is the crate's one real structural merge-capacity
//! knob:
//!
//!     k = BraidConfig::max_merge_parents   (env SIGIL_DAG_MAX_MERGE_PARENTS, default 4)
//!
//! i.e. how many concurrent foreign tips a single block may weave into the
//! braid. That makes the experiment a genuine capacity question: `P-1` foreign
//! tips are produced per round and each block can absorb at most `k` of them.
//!
//! ## The predicted critical line
//!
//! Per round, each producer receives `P-1` foreign blocks and can merge `k`.
//! The unmerged backlog is therefore a queue with arrival `P-1`, service `k`:
//!
//!     subcritical   k >  P-1   backlog drains, braid stays woven
//!     critical      k == P-1   marginal
//!     supercritical k <  P-1   backlog grows linearly at rate (P-1-k)/round
//!
//! so the predicted critical line is **k_c = P - 1**, and the predicted
//! supercritical slope is exactly `(P-1-k)` blocks/round. Both are falsifiable
//! and both are measured below — the point of the run is to check the real
//! `Braid` against them, not to assume them.
//!
//! ## Safety control (the part that must NOT transition)
//!
//! Every cell also feeds the identical DAG to two independent `Braid`
//! instances in two different arrival orders (creation order vs a seeded
//! shuffle) and compares `linearize()` position-by-position plus `order_hash`.
//! Divergence must stay 0 on BOTH sides of the transition — v1's ordering is a
//! pure function of the DAG set, so a capacity transition may cost liveness
//! (weaving throughput) but must never cost safety (order agreement). A
//! non-zero `div` column would be a real bug, not a phase.
//!
//! Run:  cargo run --release --example k_phase        (via fluxc, never raw cargo)
//! Env:  KPHASE_ROUNDS (default 400), KPHASE_SEED (default 42)

use std::collections::VecDeque;

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;

const GENESIS: BlockHash = [0u8; 32];

// ─── deterministic helpers (no clocks, no rand — bit-reproducible) ──────────

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

// ─── one measured cell ──────────────────────────────────────────────────────

struct Cell {
    producers: usize,
    k: usize,
    delay: usize,
    blocks: usize,
    /// Mean visible-but-unmerged foreign blocks per producer, over the run.
    backlog_mean: f64,
    /// Visible-but-unmerged backlog per producer at the final round.
    backlog_final: f64,
    /// Measured growth rate of that backlog, blocks/round (least-squares slope
    /// over the second half of the run, after the transient).
    backlog_slope: f64,
    /// Predicted supercritical slope, max(0, P-1-k).
    slope_predicted: f64,
    /// Fraction of minted blocks that filled all `k` merge slots.
    saturation: f64,
    /// Braid occupancy at end of run.
    tips: usize,
    window: usize,
    pending: usize,
    finalized_height: u64,
    emitted_total: usize,
    rejected: u64,
    dropped: u64,
    /// SAFETY control: positions where the two arrival orders disagree over
    /// the COMMON prefix (length differences are reported separately, so an
    /// incomplete node cannot masquerade as an ordering disagreement).
    divergence: u64,
    /// Ordered length on each node — unequal means one node lost blocks.
    len_a: usize,
    len_b: usize,
    /// Inserts refused below the finality line, per node. The suspected
    /// mechanism when a supercritical backlog exceeds `final_depth`.
    below_final_a: u64,
    below_final_b: u64,
    order_hash_match: bool,
}

/// Generate the DAG for one (P, k, D) cell and measure both the queueing
/// behaviour and the real `Braid`'s response to it.
fn run_cell(producers: usize, k: usize, delay: usize, rounds: usize, seed: u64) -> Cell {
    // Per-producer FIFO of (visible_at_round, hash) foreign blocks awaiting a
    // merge slot. Arrival is (P-1)/round, service is k/round.
    let mut backlog: Vec<VecDeque<(usize, BlockHash)>> = vec![VecDeque::new(); producers];
    let mut views: Vec<BlockView> = Vec::with_capacity(producers * rounds);
    let mut depth_series: Vec<f64> = Vec::with_capacity(rounds);
    let mut saturated = 0usize;

    for r in 1..=rounds {
        let mut minted: Vec<(usize, BlockHash)> = Vec::with_capacity(producers);

        for p in 0..producers {
            let height = r as u64;
            let hash = block_hash(p, height);
            let parent = if r == 1 { GENESIS } else { block_hash(p, height - 1) };

            // Drain up to k VISIBLE entries. Non-visible entries sit at the
            // front (FIFO by arrival), so a blocked front means nothing older
            // is servable this round — matches real gossip ordering.
            let mut merge_parents: Vec<BlockHash> = Vec::with_capacity(k);
            while merge_parents.len() < k {
                match backlog[p].front() {
                    Some(&(visible_at, h)) if visible_at <= r => {
                        backlog[p].pop_front();
                        // The Braid structurally rejects a merge parent equal
                        // to the spine parent, or a duplicate merge parent.
                        if h != parent && !merge_parents.contains(&h) {
                            merge_parents.push(h);
                        }
                    }
                    _ => break,
                }
            }
            if k > 0 && merge_parents.len() == k {
                saturated += 1;
            }

            views.push(BlockView {
                hash,
                parent,
                merge_parents,
                height,
                producer: producer_id(p),
            });
            minted.push((p, hash));
        }

        // Gossip: each minted block enters every OTHER producer's backlog,
        // becoming mergeable `delay` rounds later.
        for (origin, hash) in &minted {
            for p in 0..producers {
                if p != *origin {
                    backlog[p].push_back((r + delay, *hash));
                }
            }
        }

        // Sample the VISIBLE (mergeable-but-unmerged) depth, averaged per producer.
        let visible: usize = (0..producers)
            .map(|p| backlog[p].iter().filter(|(v, _)| *v <= r).count())
            .sum();
        depth_series.push(visible as f64 / producers as f64);
    }

    // Least-squares slope over the second half (skip the startup transient).
    let half = rounds / 2;
    let tail = &depth_series[half..];
    let n = tail.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = tail.iter().sum::<f64>() / n;
    let (mut num, mut den) = (0.0f64, 0.0f64);
    for (i, y) in tail.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    let backlog_slope = if den > 0.0 { num / den } else { 0.0 };

    // ── feed the SAME DAG to two braids in two arrival orders ──────────────
    let cfg = BraidConfig {
        final_depth: 64,
        max_window: 1 << 20,
        max_pending: 1 << 18,
        max_merge_parents: k.max(1),
        ghostdag_k: None,
        final_blue_depth: None,
    };

    let mut node_a = Braid::new_with_base(cfg.clone(), GENESIS, 0);
    for v in &views {
        node_a.insert(v.clone());
    }

    let mut shuffled: Vec<usize> = (0..views.len()).collect();
    let mut rng = XorShift64::new(seed ^ 0xD1CE_5EED_u64);
    for i in (1..shuffled.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        shuffled.swap(i, j);
    }
    let mut node_b = Braid::new_with_base(cfg, GENESIS, 0);
    for &i in &shuffled {
        node_b.insert(views[i].clone());
    }
    // EXHAUSTIVE backfill of node B. A shuffled arrival order parks blocks
    // whose parents land later; we serve the braid's own missing-parent
    // worklist (O(1) via an index map) AND then re-offer the whole view set
    // until a full pass yields no new acceptance. This removes "the harness
    // gave up early" as an explanation for any divergence measured below.
    let index: std::collections::HashMap<BlockHash, usize> =
        views.iter().enumerate().map(|(i, v)| (v.hash, i)).collect();
    let mut guard = 0usize;
    loop {
        guard += 1;
        if guard > 64 {
            break;
        }
        let mut progressed = false;
        // 1. serve the explicit worklist
        for h in node_b.missing_parents() {
            if let Some(&i) = index.get(&h) {
                if matches!(
                    node_b.insert(views[i].clone()),
                    InsertOutcome::Inserted { .. }
                ) {
                    progressed = true;
                }
            }
        }
        // 2. full re-offer sweep
        for v in &views {
            if matches!(node_b.insert(v.clone()), InsertOutcome::Inserted { .. }) {
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let lin_a = node_a.linearize();
    let lin_b = node_b.linearize();
    // Ordering disagreement over the COMMON prefix only — a length difference
    // is a completeness problem, reported in its own columns.
    let divergence = lin_a
        .iter()
        .zip(lin_b.iter())
        .filter(|(x, y)| x != y)
        .count() as u64;
    let order_hash_match = node_a.order_hash() == node_b.order_hash();
    let st_b = node_b.stats();

    let st = node_a.stats();

    Cell {
        producers,
        k,
        delay,
        blocks: views.len(),
        backlog_mean: depth_series.iter().sum::<f64>() / depth_series.len() as f64,
        backlog_final: *depth_series.last().unwrap_or(&0.0),
        backlog_slope,
        slope_predicted: ((producers as f64 - 1.0) - k as f64).max(0.0),
        saturation: saturated as f64 / views.len() as f64,
        tips: st.tips,
        window: st.window,
        pending: st.pending,
        finalized_height: st.finalized_height,
        emitted_total: st.emitted_total,
        rejected: st.rejected,
        dropped: st.dropped,
        divergence,
        len_a: lin_a.len(),
        len_b: lin_b.len(),
        below_final_a: st.below_final,
        below_final_b: st_b.below_final,
        order_hash_match,
    }
}

fn main() {
    let rounds: usize = std::env::var("KPHASE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let seed: u64 = std::env::var("KPHASE_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(42);

    eprintln!(
        "k_phase: sweeping k = BraidConfig::max_merge_parents (NOT GHOSTDAG k, NOT the Quillon k_parameter EMA)"
    );
    eprintln!("k_phase: rounds={rounds} seed={seed} — predicted critical line k_c = P-1");

    println!(
        "P\tk\tD\tblocks\tk_minus_kc\tbacklog_mean\tbacklog_final\tslope_meas\tslope_pred\tsaturation\ttips\twindow\tpending\tfinal_h\temitted\trejected\tdropped\tdiv\tlen_a\tlen_b\tbelow_final_a\tbelow_final_b\toh_match"
    );

    let producer_set = [2usize, 3, 4, 6, 8, 12, 16, 24];
    let k_set = [1usize, 2, 3, 4, 5, 6, 8, 11, 15, 23];
    let delay_set = [0usize, 2];

    for &d in &delay_set {
        for &p in &producer_set {
            for &k in &k_set {
                let c = run_cell(p, k, d, rounds, seed);
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.4}\t{:.1}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    c.producers,
                    c.k,
                    c.delay,
                    c.blocks,
                    c.k as i64 - (c.producers as i64 - 1), // distance from the predicted critical line
                    c.backlog_mean,
                    c.backlog_final,
                    c.backlog_slope,
                    c.slope_predicted,
                    c.saturation,
                    c.tips,
                    c.window,
                    c.pending,
                    c.finalized_height,
                    c.emitted_total,
                    c.rejected,
                    c.dropped,
                    c.divergence,
                    c.len_a,
                    c.len_b,
                    c.below_final_a,
                    c.below_final_b,
                    c.order_hash_match,
                );
            }
        }
    }
}
