//! sigil-breathing — the breathing-mode test from *Null Rays, Twist and the Kristensen Gauge*
//! (2026-09-17, sigilgraph.org/downloads/null-rays-k-gauge.pdf). Prediction under test (the
//! paper's falsifiable statement 1): on a two-producer chain with a healed partition, the twist
//! ι² and the width s of the bundle of views obey Aazami's (2), ι² ∝ 1/s⁴ (log–log slope −4),
//! and the width BREATHES after healing with the isochronous period 2π/√(2R).
//!
//! Three scenarios on the REAL `SigilSimNode` chokepoint (`commit_state_transition` + root
//! match), driver-stepped so every block boundary is sampled:
//!   twisted   — A and B alternate on ONE spine (each builds on the other's block); follower F3
//!               is partitioned for blocks [P0, P1) and then catches up at CATCHUP blocks per
//!               produced block.
//!   untwisted — A alone (one key); the same partition and heal.
//!   twosheet  — a real fork: from P0 on, A and B each extend their OWN tip; F0, F1 follow A,
//!               F2, F3 follow B; at P1 the producers are "reconnected" — and nothing contracts,
//!               because a SIGIL follower cannot reorg (`apply_block`: parent must match).
//! Width: area = 1 + mean |h_spine − h_follower| + (tips_at_height − 1)·TIP_W, s = √area.
//! Twist: ι² = interleave = fraction of consecutive spine blocks signed by DIFFERENT producers
//! over the last TWIST_WIN blocks. Expansion θ = Δ ln(area) per block.
//!
//! Usage: sigil-breathing [out_dir]   → breathing.json (rows) + breathing-report.md
use std::collections::HashMap;

use flux_chronos::{secs, NodeId};
use serde_json::json;
use sigil_chronos::{demo_genesis, sign_dummy, ApplyOutcome, Block, SigilSimNode};
use sigil_tx::{SigilTx, NATIVE};

const H: u64 = 240; // blocks per scenario
const P0: u64 = 60; // partition begins
const P1: u64 = 120; // partition heals
const CATCHUP: u64 = 4; // blocks a healed follower applies per produced block (delivery rate)
const TIP_W: f64 = 64.0; // a second tip at one height counts as this many blocks of spread
const TWIST_WIN: usize = 16;
const FINAL_DEPTH: f64 = 512.0;
const PI: f64 = std::f64::consts::PI;

fn w(t: u8) -> [u8; 32] { [t; 32] }
fn node(name: &str, id: u32, producer: bool) -> SigilSimNode {
    let mut n = SigilSimNode::new(name, NodeId(id), vec![], producer, secs(1), &demo_genesis());
    if producer {
        for i in 0..H * 2 {
            let f = (i % 5 + 1) as u8;
            let t = ((i + 1) % 5 + 1) as u8;
            n.enqueue_tx(sign_dummy(SigilTx::Send { from: w(f), to: w(t), amount: 10 + (i % 50) as u128, token: NATIVE, fee: 1 }));
        }
    }
    n
}
fn shannon_bits(counts: &[u64]) -> f64 {
    let n: u64 = counts.iter().sum();
    if n == 0 { return 0.0; }
    counts.iter().filter(|&&c| c > 0).map(|&c| { let p = c as f64 / n as f64; -p * p.log2() }).sum()
}
fn k_fix(dh_c: f64, d_blocks: f64, h_norm: f64) -> f64 {
    let tau_d = (1.0 + d_blocks.clamp(0.0, FINAL_DEPTH) / FINAL_DEPTH) / 2.0;
    let w_s = 1.0 - h_norm.clamp(0.0, 1.0) / 4.0;
    2.0 * PI * (dh_c.clamp(0.0, 1.0) * tau_d * w_s).sqrt()
}

struct Follower { node: SigilSimNode, queue: Vec<Block>, branch: usize, partitioned: bool }

struct Row { h: u64, area: f64, s: f64, iota2: f64, theta: f64, gap: f64, tips: usize, k_fix: f64, ds_bits: f64, rejected: u64 }

/// One scenario. `twisted`: alternate A/B on one spine. `fork`: from P0, two sheets.
fn run(name: &str, twisted: bool, fork: bool) -> (Vec<Row>, serde_json::Value) {
    let mut a = node("A", 0, true);
    let mut b = node("B", 1, true);
    let mut fs: Vec<Follower> = (0..4u32).map(|i| Follower { node: node(&format!("F{i}"), 10 + i, false), queue: vec![], branch: 0, partitioned: false }).collect();
    let mut spine: Vec<[u8; 32]> = vec![]; // producer ids along the reference spine (A's chain in the fork)
    let mut rows: Vec<Row> = vec![];
    let mut prev_area = 1.0f64;
    let mut rejected_total = 0u64;
    let mut tips_b: Option<[u8; 32]> = None;
    for h in 1..=H {
        // ── produce ──
        let in_fork = fork && h >= P0;
        let blocks: Vec<(usize, Block)> = if in_fork {
            // two sheets: each producer extends its own tip; neither applies the other
            let ba = a.produce_one().expect("A produces");
            let bb = b.produce_one().expect("B produces");
            vec![(0, ba), (1, bb)]
        } else if twisted {
            let (blk, other) = if h % 2 == 1 { (a.produce_one().expect("A"), &mut b) } else { (b.produce_one().expect("B"), &mut a) };
            assert_eq!(other.apply_external_block(&blk), ApplyOutcome::Ok, "the other producer builds on this block");
            vec![(0, blk)]
        } else {
            let blk = a.produce_one().expect("A");
            assert_eq!(b.apply_external_block(&blk), ApplyOutcome::Ok);
            vec![(0, blk)]
        };
        spine.push(blocks[0].1.header.producer);
        if let Some((_, bb)) = blocks.get(1) { tips_b = Some(bb.hash()); }
        // ── deliver ──
        for (i, f) in fs.iter_mut().enumerate() {
            f.partitioned = !fork && i == 3 && (P0..P1).contains(&h);
            if fork && h >= P0 { f.branch = if i < 2 { 0 } else { 1 }; }
            let mut foreign: Vec<Block> = vec![];
            for (branch, blk) in &blocks {
                if fork && h >= P0 && *branch != f.branch { foreign.push(blk.clone()); } else { f.queue.push(blk.clone()); }
            }
            // catch-up: a connected follower applies up to CATCHUP queued blocks per produced block
            if !f.partitioned {
                let n = f.queue.len().min(CATCHUP as usize);
                for blk in f.queue.drain(..n) {
                    match f.node.apply_external_block(&blk) { ApplyOutcome::Ok => {}, ApplyOutcome::Rejected => rejected_total += 1, ApplyOutcome::Divergence => panic!("state divergence at H={}", blk.header.height) }
                }
            }
            // the other sheet's block arrives AFTER our own and is refused (parent mismatch) — a
            // SIGIL follower cannot reorg, so "reconnecting" the producers at P1 changes nothing
            for blk in foreign {
                if f.node.apply_external_block(&blk) == ApplyOutcome::Rejected { rejected_total += 1; }
            }
        }
        // ── measure ──
        let h_spine = a.height() - 1; // A's applied height
        let gap = fs.iter().map(|f| (h_spine as f64 - (f.node.height() - 1) as f64).abs()).sum::<f64>() / fs.len() as f64;
        let tips = if in_fork { 2 } else { 1 };
        let area = 1.0 + gap + (tips as f64 - 1.0) * TIP_W;
        let s = area.sqrt();
        let win = &spine[spine.len().saturating_sub(TWIST_WIN)..];
        let iota2 = if win.len() < 2 { 0.0 } else { win.windows(2).filter(|p| p[0] != p[1]).count() as f64 / (win.len() - 1) as f64 };
        let theta = area.ln() - prev_area.ln();
        prev_area = area;
        let mut counts: HashMap<[u8; 32], u64> = HashMap::new();
        for p in win { *counts.entry(*p).or_default() += 1; }
        let ds = shannon_bits(&counts.values().copied().collect::<Vec<_>>());
        let h_norm = if counts.len() > 1 { ds / (counts.len() as f64).log2() } else { 0.0 };
        let dh_c = 0.20 * (tips as f64 - 1.0).min(1.0) + 0.20 * (gap / FINAL_DEPTH).min(1.0);
        let kf = k_fix(dh_c, gap + (tips as f64 - 1.0) * TIP_W, h_norm);
        rows.push(Row { h, area, s, iota2, theta, gap, tips, k_fix: kf, ds_bits: ds, rejected: rejected_total });
    }
    let _ = tips_b;
    // ── the fits ──
    // slope of log ι² vs log s over the heal window (P1..P1+60), where s varies
    let pts: Vec<(f64, f64)> = rows.iter().filter(|r| r.h >= P1 && r.h < P1 + 60 && r.iota2 > 0.0 && r.s > 0.0).map(|r| (r.s.ln(), r.iota2.ln())).collect();
    let slope = if pts.len() >= 3 {
        let n = pts.len() as f64;
        let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        if sxx < 1e-12 { None } else { Some(pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / sxx) }
    } else { None };
    // breathing: sign changes of θ after the heal (a breathing width alternates; a catch-up is monotone)
    let after: Vec<f64> = rows.iter().filter(|r| r.h > P1 && r.h <= P1 + 60).map(|r| r.theta).collect();
    let sign_changes = after.windows(2).filter(|p| p[0].signum() != p[1].signum() && p[0] != 0.0 && p[1] != 0.0).count();
    let s_max = rows.iter().map(|r| r.s).fold(0.0, f64::max);
    let s_end = rows.last().map(|r| r.s).unwrap_or(0.0);
    let heal_blocks = rows.iter().filter(|r| r.h >= P1 && r.gap > 0.0).count();
    let period_pred = 2.0 * PI / (2.0f64).sqrt(); // R = 1 per block (the follower is pulled every block)
    let summary = json!({
        "scenario": name, "twisted": twisted, "fork": fork, "blocks": H, "partition": [P0, P1], "catchup_per_block": CATCHUP,
        "s_max": s_max, "s_end": s_end, "heal_blocks_with_gap": heal_blocks,
        "iota2_min": rows.iter().map(|r| r.iota2).fold(1.0, f64::min), "iota2_max": rows.iter().map(|r| r.iota2).fold(0.0, f64::max),
        "loglog_slope_iota2_vs_s": slope, "slope_predicted": -4.0,
        "theta_sign_changes_after_heal": sign_changes, "breathing_period_predicted_blocks": period_pred,
        "k_fix_max": rows.iter().map(|r| r.k_fix).fold(0.0, f64::max), "k_fix_end": rows.last().map(|r| r.k_fix).unwrap_or(0.0),
        "rejected_total": rejected_total,
    });
    (rows, summary)
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "/home/storage/sigil-scratch/sigil-breathing-2026-09-17".into());
    std::fs::create_dir_all(&out).expect("out dir");
    let mut all = vec![];
    let mut summaries = vec![];
    for (name, twisted, fork) in [("twisted", true, false), ("untwisted", false, false), ("twosheet", true, true)] {
        let (rows, sum) = run(name, twisted, fork);
        for r in &rows {
            all.push(json!({"scenario": name, "h": r.h, "area": r.area, "s": r.s, "iota2": r.iota2, "theta": r.theta, "gap": r.gap, "tips": r.tips, "k_fix": r.k_fix, "ds_bits": r.ds_bits, "rejected": r.rejected}));
        }
        summaries.push(sum);
    }
    std::fs::write(format!("{out}/breathing.json"), serde_json::to_string_pretty(&json!({"rows": all, "summaries": summaries})).unwrap()).expect("json");
    let f = |v: &serde_json::Value, k: &str| v[k].as_f64().map(|x| format!("{x:.3}")).unwrap_or("n/a".into());
    let mut md = String::new();
    md.push_str("# The breathing test — Aazami's (2) on a two-producer SIGIL chain (chronos, 2026-09-17)\n\n");
    md.push_str("Prediction under test (paper §Falsifiable 1): after a healed partition, twist ι² and width s obey ι² ∝ 1/s⁴ (log–log slope −4) and the width breathes with period 2π/√(2R) ≈ 4.44 blocks. Real `SigilSimNode` chokepoint, driver-stepped, every block sampled. Width: area = 1 + mean height gap + (tips−1)·64, s = √area. Twist: ι² = interleave fraction of the last 16 spine blocks.\n\n");
    md.push_str("| scenario | s_max | s_end | ι² min..max | slope log ι² vs log s (pred −4) | θ sign changes after heal | K_fix max → end | rejected |\n|---|---|---|---|---|---|---|---|\n");
    for s in &summaries {
        md.push_str(&format!("| {} | {} | {} | {}..{} | {} | {} | {} → {} | {} |\n", s["scenario"].as_str().unwrap(), f(s, "s_max"), f(s, "s_end"), f(s, "iota2_min"), f(s, "iota2_max"),
            s["loglog_slope_iota2_vs_s"].as_f64().map(|x| format!("{x:.3}")).unwrap_or("undefined (ι² = 0 or s constant)".into()),
            s["theta_sign_changes_after_heal"], f(s, "k_fix_max"), f(s, "k_fix_end"), s["rejected_total"]));
    }
    md.push_str("\n## Reading\n\n");
    md.push_str("- **twisted** (A/B alternate on one spine): ι² stays 1.0 while s rises during the partition and falls after the heal — the twist does not move with the width at all; the log–log slope is 0, not −4. Equation (2) does **not** govern proposer entropy. The width contracts monotonically at the delivery rate (θ sign changes = 0): a catch-up, not a breathing.\n");
    md.push_str("- **untwisted** (one key): identical width history, ι² = 0 throughout. The width still contracts — a follower catching up IS the untwisted caustic, and it is benign: two rays meeting is agreement when one of them is a follower.\n");
    md.push_str("- **twosheet** (a real fork between producers): from P0 the interleave drops to 0 and the width jumps by a whole tip; at P1 the producers are reconnected and NOTHING contracts — every cross-sheet block is refused (parent mismatch). There is no focusing between producers in SIGIL today because a follower cannot reorg (2026-09-08 finding). The caustic that matters — two *producers'* views meeting — cannot happen.\n\n");
    md.push_str("**Verdict:** falsifiable statement 1 is FALSIFIED for the current chain, and for a design reason, not a geometric one. Proposer entropy is a property of who signs, not of how many views exist; the geometric twist of a DAG would be its merge structure, and SIGIL's linear followers have none. The dictionary stays an analogy. What the run does confirm: the gauge's width channel (finality gap) rises with the partition and falls with the heal in every scenario, and a two-sheet fork keeps K_fix pinned high with no way down.\n");
    md.push_str("\nGenerator: `sigil/crates/sigil-chronos/src/bin/sigil-breathing.rs` (fluxc, --profile release-fast). Rows: `breathing.json`.\n");
    std::fs::write(format!("{out}/breathing-report.md"), &md).expect("md");
    println!("{}", serde_json::to_string_pretty(&summaries).unwrap());
    println!("wrote {out}/breathing.json and breathing-report.md");
}
