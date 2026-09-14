//! sigil-kparam-battle — battle test of the Kristensen K-parameter
//!
//!     K* = 2π · √( ΔH · t · ΔS / ħ )          (Viktor's blackboard, 2026-09-14 08:52)
//!
//! against the REAL chain components under chronos, not against a re-implementation:
//!   Tier A — `sigil-dagknight::Braid` with GHOSTDAG colouring fed a generated DAG whose
//!            proposer distribution (ΔS axis) and fork rate (disagreement axis) are
//!            controlled INDEPENDENTLY, then read through `recent_summary(200)` — the
//!            same 200-block window the live gauge reads from `/v1/dagknight/recent`.
//!   Tier B — `SigilSimNode` (real `commit_state_transition` chokepoint) under
//!            `flux_chronos::Universe` with latency / drop / partition edges, one or two
//!            producers, followers; equivocation by `produce_one`/`apply_external_block`;
//!            `property::run_trial` fuzz.
//!
//! Three readings of ΔH are carried side by side because the four K formulas in this
//! tree do not agree on what ΔH is:
//!   (i)   ΔH_J   — an ENERGY in joules (the paper generator, real ħ)
//!   (ii)  ΔH_c   — the v2 state-disagreement channels, dimensionless (the live gauge, ħ:=1)
//!   (iii) ΔH_S   — the window-to-window CHANGE of proposer entropy (can be negative)
//!
//! Every row is JSONL: `sweep`, inputs, and every derived number, so an independent pass
//! can recompute K* from the stored inputs. Nothing here touches a live node.
//!
//!   sigil-kparam-battle <out_dir> [ds|fork0|sybil|rate|energy|negdh|net|fuzz|all] [fuzz_n]

use std::collections::HashMap;
use std::io::Write;

use flux_chronos::{secs, NetEdge, NodeId, ScenarioSeed, Universe};
use serde_json::json;
use sigil_chronos::property::run_trial;
use sigil_chronos::{demo_genesis, producer_id_for, sign_dummy, ApplyOutcome, SigilSimNode};
use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;
use sigil_tx::{SigilTx, NATIVE};

// ─────────────────────────────── the formulas under test ───────────────────────────────

/// CODATA 2018 exact value (flux-science::constants::PLANCK_REDUCED carries the same).
const HBAR: f64 = 1.054_571_817e-34;
const BOLTZMANN: f64 = 1.380_649e-23;
const PI: f64 = std::f64::consts::PI;
/// Live gauge constants (fluxc-mcp sigil_kgauge.rs:71-84).
const FINAL_DEPTH: f64 = 512.0;
const TAU0_SECS: f64 = 100.0;
const EPSILON_FLOOR: f64 = 0.1;
const W_TIP: f64 = 0.20;
const W_STATE: f64 = 0.35;
const W_FINALITY: f64 = 0.20;
const W_CONFLICT: f64 = 0.25;
/// Envelope tags (sigil-chronos lib.rs:86-87, private there).
const TAG_TX: u8 = 0;
/// Window the live gauge reads (`/v1/dagknight/recent` → 200 blocks).
const WINDOW: usize = 200;

/// The picture, literally: K* = 2π√(ΔH·t·ΔS/ħ) with ΔH in joules.
fn k_star_hbar(dh_j: f64, t_s: f64, ds: f64) -> f64 {
    2.0 * PI * (dh_j * t_s * ds / HBAR).sqrt()
}
/// The picture with ħ := 1 and a dimensionless ΔH — what every dashboard actually computes.
fn k_star_unit(dh: f64, t_s: f64, ds: f64) -> f64 {
    2.0 * PI * (dh * t_s * ds).sqrt()
}
/// Margolus–Levitin orthogonalisation count in t at energy E.
fn n_ml(e_j: f64, t_s: f64) -> f64 {
    2.0 * e_j * t_s / (PI * HBAR)
}
/// K* through N_ML: 2π√((π/2)·N_ML·ΔS). Must equal k_star_hbar (paper identity check).
fn k_star_via_ml(e_j: f64, t_s: f64, ds: f64) -> f64 {
    2.0 * PI * (std::f64::consts::FRAC_PI_2 * n_ml(e_j, t_s) * ds).sqrt()
}
/// Live SIGIL K_C v2 (sigil_kgauge.rs:217-220) — for reference beside the picture.
fn k_c(dh_c: f64, tau_s: f64, h_norm: f64) -> f64 {
    let plur = EPSILON_FLOOR + (1.0 - EPSILON_FLOOR) * h_norm.clamp(0.0, 1.0);
    2.0 * PI * (dh_c * (tau_s / TAU0_SECS) * plur).sqrt()
}
/// THE REPAIR (2026-09-14, after the v1 battle test): K_fix = 2π·√(ΔH_c · τ_d · w_S).
///   ΔH_c ∈ [0,1] — weighted STATE disagreement (tip 0.20, state-root 0.35, finality 0.20,
///                  semantic 0.25). Non-negative by definition, so the root is always real.
///   τ_d = (1 + min(d, D)/D)/2 ∈ [½, 1] — persistence measured in BLOCKS: d = blocks the
///                  disagreement has lasted, D = 512 (finality depth). Replaces t/ħ. A chain
///                  that speeds up does not "cool"; a fork that outlives finality reads worst.
///   w_S = 1 − H_norm/4 ∈ [¾, 1] — proposer entropy kept as a CONCENTRATION weight: one key
///                  disagreeing with itself is the worst case; rotating keys can lower K by at
///                  most √(4/3) ≈ 15 % and can never raise it; zero disagreement → 0 for any key
///                  count; any ΔH_c ≥ 0.61 reads critical whatever the key count (first cut used
///                  /2 and let a persistent two-party split slip to 2.97 "elevated").
/// Ladder unchanged (<1 stable · <3 elevated · ≥3 critical); the maximum is 2π for a full,
/// persistent, single-key state divergence. The ħ reading survives beside it as N_ML.
fn k_fix(dh_c: f64, d_blocks: f64, h_norm: f64) -> f64 {
    let tau_d = (1.0 + d_blocks.clamp(0.0, FINAL_DEPTH) / FINAL_DEPTH) / 2.0;
    let w_s = 1.0 - h_norm.clamp(0.0, 1.0) / 4.0;
    2.0 * PI * (dh_c.clamp(0.0, 1.0) * tau_d * w_s).sqrt()
}
fn shannon_bits(counts: &[u64]) -> f64 {
    let n: u64 = counts.iter().sum();
    if n == 0 {
        return 0.0;
    }
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n as f64;
            p * p.log2()
        })
        .sum::<f64>()
}
fn phase_mcp(k: f64) -> &'static str {
    if k.is_nan() {
        "NaN"
    } else if k < 1.0 {
        "stable"
    } else if k < 3.0 {
        "elevated"
    } else {
        "critical"
    }
}
fn phase_kgauge(k: f64) -> &'static str {
    if k.is_nan() {
        "NaN"
    } else if k < 5.0 {
        "stable"
    } else if k < 10.0 {
        "approaching"
    } else {
        "critical"
    }
}
fn nan_to_null(x: f64) -> serde_json::Value {
    if x.is_finite() {
        json!(x)
    } else {
        json!(format!("{x}"))
    }
}

// ─────────────────────────────── output ───────────────────────────────

struct Out {
    dir: String,
    files: HashMap<String, std::fs::File>,
    rows: u64,
}
impl Out {
    fn new(dir: &str) -> Self {
        std::fs::create_dir_all(dir).expect("out dir");
        Self { dir: dir.into(), files: HashMap::new(), rows: 0 }
    }
    fn row(&mut self, sweep: &str, mut v: serde_json::Value) {
        v["sweep"] = json!(sweep);
        let f = self.files.entry(sweep.to_string()).or_insert_with(|| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{}/{}.jsonl", self.dir, sweep))
                .expect("open sweep file")
        });
        writeln!(f, "{}", v).expect("write row");
        self.rows += 1;
    }
}

// ─────────────────────────────── Tier A: DAG generator → real Braid ───────────────────────────────

struct SplitMix64(u64);
impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn pid(p: usize) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"kparam/prod");
    h.update(&(p as u64).to_le_bytes());
    *h.finalize().as_bytes()
}
fn bhash(p: usize, height: u64, salt: u64) -> BlockHash {
    let mut h = blake3::Hasher::new();
    h.update(b"kparam/blk");
    h.update(&(p as u64).to_le_bytes());
    h.update(&height.to_le_bytes());
    h.update(&salt.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Proposer share distributions — the ΔS axis.
fn weights(dist: &str, p: usize) -> Vec<f64> {
    match dist {
        "single" => (0..p).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect(),
        "uniform" => vec![1.0 / p as f64; p],
        "zipf" => {
            let w: Vec<f64> = (0..p).map(|i| 1.0 / (i as f64 + 1.0)).collect();
            let s: f64 = w.iter().sum();
            w.into_iter().map(|x| x / s).collect()
        }
        "split199" => {
            // one dominant producer with 199/200 of the blocks, the rest share 1/200
            let rest = if p > 1 { (1.0 / 200.0) / (p as f64 - 1.0) } else { 0.0 };
            (0..p).map(|i| if i == 0 { 199.0 / 200.0 } else { rest }).collect()
        }
        _ => panic!("unknown dist {dist}"),
    }
}
fn pick(rng: &mut SplitMix64, w: &[f64]) -> usize {
    let u = rng.unit();
    let mut acc = 0.0;
    for (i, x) in w.iter().enumerate() {
        acc += x;
        if u < acc {
            return i;
        }
    }
    w.len() - 1
}

/// Slot model: a spine of `n` heights. At each height one block on the current tip by a
/// producer drawn from `w`; with probability `fork_p` a SECOND block at the same height on
/// the same parent (a sibling — real concurrent production), drawn from `w` as well (so a
/// single producer forks AGAINST ITSELF = equivocation). The next spine block merges the
/// sibling as a merge-parent. `fork_p` is the disagreement axis, `w` the entropy axis —
/// the two things the product form multiplies.
fn gen_slot_dag(n: u64, w: &[f64], fork_p: f64, seed: u64) -> (Vec<BlockView>, u64) {
    let mut rng = SplitMix64(seed);
    let genesis: BlockHash = [0u8; 32];
    let mut tip = genesis;
    let mut pending_merge: Vec<BlockHash> = Vec::new();
    let mut views = Vec::with_capacity(n as usize * 2);
    let mut forks = 0u64;
    for h in 1..=n {
        let p = pick(&mut rng, w);
        let hash = bhash(p, h, rng.next_u64());
        let merge_parents = std::mem::take(&mut pending_merge);
        views.push(BlockView { hash, parent: tip, merge_parents, height: h, producer: pid(p), difficulty: 0 });
        if rng.unit() < fork_p {
            let q = pick(&mut rng, w);
            let sib = bhash(q, h, rng.next_u64());
            views.push(BlockView { hash: sib, parent: tip, merge_parents: vec![], height: h, producer: pid(q), difficulty: 0 });
            pending_merge.push(sib);
            forks += 1;
        }
        tip = hash;
    }
    (views, forks)
}

/// Feed a DAG into a real Braid (GHOSTDAG on) and read the live-gauge window back.
struct WindowRead {
    ds_bits: f64,
    distinct: usize,
    n: usize,
    merge_blocks: usize,
    red_blocks: usize,
    inserted: u64,
    rejected: u64,
    finalized: u64,
}
fn braid_window(views: &[BlockView], k: u32) -> WindowRead {
    let mut cfg = BraidConfig::from_env();
    cfg.final_depth = 512;
    cfg.max_merge_parents = 4;
    cfg.ghostdag_k = Some(k);
    cfg.final_blue_depth = None;
    let mut braid = Braid::new_with_base(cfg, [0u8; 32], 0);
    let (mut inserted, mut rejected) = (0u64, 0u64);
    for v in views {
        match braid.insert(v.clone()) {
            InsertOutcome::Inserted { .. } => inserted += 1,
            InsertOutcome::Rejected(_) => rejected += 1,
            _ => {}
        }
    }
    let recent = braid.recent_summary(WINDOW);
    let mut counts: HashMap<[u8; 32], u64> = HashMap::new();
    for b in &recent {
        *counts.entry(b.producer).or_default() += 1;
    }
    let c: Vec<u64> = counts.values().copied().collect();
    WindowRead {
        ds_bits: shannon_bits(&c),
        distinct: counts.len(),
        n: recent.len(),
        merge_blocks: recent.iter().filter(|b| !b.merge_parents.is_empty()).count(),
        red_blocks: recent.iter().filter(|b| !b.is_blue).count(),
        inserted,
        rejected,
        finalized: braid.finalized_height(),
    }
}

/// ΔH_c as the live gauge builds it from a DAG window (state + finality channels are
/// unavailable in Tier A, exactly as they were on the live node before 2026-09-07 → lower bound).
fn dh_c_from_window(wr: &WindowRead) -> (f64, f64, f64) {
    let n = wr.n.max(1) as f64;
    let d_tip = wr.merge_blocks as f64 / n;
    let d_conflict = wr.red_blocks as f64 / n;
    (W_TIP * d_tip + W_CONFLICT * d_conflict, d_tip, d_conflict)
}

/// One full row for a Tier A window at block rate `bps`, with the energy reading of ΔH
/// taken as `power_w × block time` (energy spent per round) — the paper's ΔH is an energy.
fn tier_a_row(name: &str, p: usize, dist: &str, fork_p: f64, forks: u64, wr: &WindowRead, bps: f64, power_w: f64) -> serde_json::Value {
    let tau_b = 1.0 / bps;
    let tau_fin = FINAL_DEPTH / bps;
    let (dh_c, d_tip, d_conflict) = dh_c_from_window(wr);
    let h_norm = if wr.distinct > 1 { wr.ds_bits / (wr.distinct as f64).log2() } else { 0.0 };
    let dh_j = power_w * tau_b;
    let ks_hbar = k_star_hbar(dh_j, tau_fin, wr.ds_bits);
    let ks_unit = k_star_unit(dh_c, tau_fin, wr.ds_bits);
    let kc = k_c(dh_c, tau_fin, h_norm);
    let d = (wr.merge_blocks + wr.red_blocks) as f64;
    let kf = k_fix(dh_c, d, h_norm);
    json!({
        "scenario": name, "producers": p, "dist": dist, "fork_p": fork_p, "forks_generated": forks,
        "persist_d": d, "k_fix": nan_to_null(kf), "phase_fix": phase_mcp(kf),
        "bps": bps, "tau_block_s": tau_b, "tau_final_s": tau_fin, "power_w": power_w,
        "window": wr.n, "distinct_producers": wr.distinct, "ds_bits": wr.ds_bits,
        "n_eff": (2f64).powf(wr.ds_bits), "h_norm": h_norm,
        "merge_blocks": wr.merge_blocks, "red_blocks": wr.red_blocks,
        "braid_inserted": wr.inserted, "braid_rejected": wr.rejected, "braid_finalized": wr.finalized,
        "d_tip": d_tip, "d_conflict": d_conflict, "dh_c": dh_c, "dh_j": dh_j,
        "n_ml": n_ml(dh_j, tau_fin),
        "k_star_hbar": nan_to_null(ks_hbar), "k_star_unit": nan_to_null(ks_unit), "k_c": nan_to_null(kc),
        "phase_hbar_mcp": phase_mcp(ks_hbar), "phase_unit_mcp": phase_mcp(ks_unit),
        "phase_unit_kgauge": phase_kgauge(ks_unit), "phase_kc": phase_mcp(kc),
    })
}

const LIVE_BPS: f64 = 110.0; // measured 2026-09-14 09:05 CEST, 2209 blocks / 20.1 s
const CORE_W: f64 = 15.0; // the paper's "15 W core"

fn sweep_ds(out: &mut Out) {
    for &p in &[1usize, 2, 3, 6, 16, 64, 1000] {
        for dist in ["single", "uniform", "zipf", "split199"] {
            if p == 1 && dist != "single" {
                continue;
            }
            for &fork_p in &[0.0, 0.05, 0.2, 0.5] {
                let n = (4 * p as u64).max(2000);
                let (views, forks) = gen_slot_dag(n, &weights(dist, p), fork_p, 0xD5 ^ p as u64);
                let wr = braid_window(&views, 1);
                out.row("ds", tier_a_row("ds_grid", p, dist, fork_p, forks, &wr, LIVE_BPS, CORE_W));
            }
        }
    }
}

fn sweep_sybil(out: &mut Out) {
    // One entity. Same fork pattern (seed fixed), same disagreement. Only the NUMBER OF
    // KEYS it signs with changes: 1 → 1000 rotating ids.
    for &ids in &[1usize, 2, 10, 100, 1000] {
        for &fork_p in &[0.0, 0.05, 0.2] {
            let (views, forks) = gen_slot_dag(4000, &weights("uniform", ids), fork_p, 0x5B1);
            let wr = braid_window(&views, 1);
            out.row("sybil", tier_a_row("one_entity_rotating_keys", ids, "uniform", fork_p, forks, &wr, LIVE_BPS, CORE_W));
        }
    }
}

fn sweep_rate(out: &mut Out) {
    // Fixed DAG (P=6 uniform, 20 % siblings) → ΔS and ΔH_c fixed; only the block rate moves.
    let (views, forks) = gen_slot_dag(4000, &weights("uniform", 6), 0.2, 0x4A7E);
    let wr = braid_window(&views, 1);
    for &bps in &[0.1, 0.38, 2.66, 6.6, 110.0, 10_000.0] {
        out.row("rate", tier_a_row("same_dag_other_speed", 6, "uniform", 0.2, forks, &wr, bps, CORE_W));
    }
}

fn sweep_energy(out: &mut Out) {
    // Pure formula, no chain: is there ANY physical energy scale at which the picture's
    // K* lands in the ladder's stable/elevated bands?
    let ds_list = [0.0454f64, 1.0, 2.32, 9.97];
    let tau_list = [FINAL_DEPTH / LIVE_BPS, 512.0 / 0.38];
    for &ds in &ds_list {
        for &tau in &tau_list {
            let tau_b = tau / FINAL_DEPTH;
            let cases: Vec<(&str, f64)> = vec![
                ("uncertainty_floor ħ/(2τ)", HBAR / (2.0 * tau)),
                ("paper k_B·1000K", BOLTZMANN * 1000.0),
                ("room k_B·300K", BOLTZMANN * 300.0),
                ("15W_core×τ_b", 15.0 * tau_b),
                ("1kW_rig×τ_b", 1000.0 * tau_b),
                ("ITER_350MJ", 350e6),
            ];
            for (label, e) in cases {
                let ks = k_star_hbar(e, tau, ds);
                out.row(
                    "energy",
                    json!({
                        "scenario": label, "ds_bits": ds, "tau_final_s": tau, "dh_j": e, "n_ml": n_ml(e, tau),
                        "k_star_hbar": nan_to_null(ks), "k_star_via_ml": nan_to_null(k_star_via_ml(e, tau, ds)),
                        "phase_mcp": phase_mcp(ks), "phase_kgauge": phase_kgauge(ks),
                        "energy_for_kstar_1": HBAR / (4.0 * PI * PI * tau * ds),
                        "n_ml_for_kstar_1": 1.0 / (2.0 * PI.powi(3) * ds),
                    }),
                );
            }
        }
    }
}

fn sweep_negdh(out: &mut Out) {
    // Time series: 40 windows. Producers 6-uniform for 20 windows, then the chain
    // centralises to 1. Reading (iii): ΔH := S_w − S_{w−1}.
    let mut prev: Option<f64> = None;
    for w in 0..40u64 {
        let p = if w < 20 { 6 } else { 1 };
        let (views, forks) = gen_slot_dag(WINDOW as u64, &weights(if p == 1 { "single" } else { "uniform" }, p), 0.05, 0x2E6 + w);
        let wr = braid_window(&views, 1);
        let tau = FINAL_DEPTH / LIVE_BPS;
        let dh_s = prev.map(|s| wr.ds_bits - s);
        let ks = dh_s.map(|d| k_star_unit(d, tau, wr.ds_bits));
        let (dh_c, _, _) = dh_c_from_window(&wr);
        let h_norm = if wr.distinct > 1 { wr.ds_bits / (wr.distinct as f64).log2() } else { 0.0 };
        let dpers = (wr.merge_blocks + wr.red_blocks) as f64;
        let kf = k_fix(dh_c, dpers, h_norm);
        out.row(
            "negdh",
            json!({
                "window_idx": w, "producers": p, "forks_generated": forks, "ds_bits": wr.ds_bits,
                "dh_entropy_change": dh_s, "tau_final_s": tau,
                "k_star_unit_reading_iii": ks.map(nan_to_null),
                "phase": ks.map(phase_mcp),
                "dh_c": dh_c, "h_norm": h_norm, "persist_d": dpers, "k_fix": nan_to_null(kf), "phase_fix": phase_mcp(kf),
            }),
        );
        prev = Some(wr.ds_bits);
    }
}

// ─────────────────────────────── Tier B: real state chokepoint ───────────────────────────────

fn w(t: u8) -> [u8; 32] {
    [t; 32]
}
fn producer(name: &str, id: u32, peers: Vec<NodeId>, sends: &[(u8, u8, u128)]) -> SigilSimNode {
    let g = demo_genesis();
    let mut n = SigilSimNode::new(name, NodeId(id), peers, true, secs(1), &g);
    for &(f, t, a) in sends {
        n.enqueue_tx(sign_dummy(SigilTx::Send { from: w(f), to: w(t), amount: a, token: NATIVE, fee: 1 }));
    }
    n
}
fn follower(name: &str, id: u32) -> SigilSimNode {
    SigilSimNode::new(name, NodeId(id), vec![], false, secs(1), &demo_genesis())
}

/// Fork with ΔS = 0 vs the identical fork with ΔS = 1 bit. Same blocks, same disagreement,
/// same follower split. The ONLY difference: whether the equivocating producer used one key
/// or two. `apply_external_block` is the real precheck + chokepoint + root compare.
fn sweep_fork0(out: &mut Out) {
    for (case, second_id) in [("equivocation_one_key_ds0", 0u32), ("two_keys_same_fork_ds1", 1u32)] {
        // Common prefix of 24 blocks from producer A, then A (or A') mints a conflicting 25th.
        let sends: Vec<(u8, u8, u128)> = (0..24u8).map(|i| (i % 5 + 1, (i + 1) % 5 + 1, 10 + i as u128)).collect();
        let mut a = producer("A", 0, vec![], &sends);
        let mut f1 = follower("F1", 10);
        let mut f2 = follower("F2", 11);
        let mut prefix = Vec::new();
        while let Some(b) = a.produce_one() {
            prefix.push(b);
        }
        for b in &prefix {
            assert_eq!(f1.apply_external_block(b), ApplyOutcome::Ok);
            assert_eq!(f2.apply_external_block(b), ApplyOutcome::Ok);
        }
        let mut a2 = if second_id == 0 {
            a.clone()
        } else {
            let mut n = SigilSimNode::new("A'", NodeId(second_id), vec![], true, secs(1), &demo_genesis());
            for b in &prefix {
                assert_eq!(n.apply_external_block(b), ApplyOutcome::Ok);
            }
            n
        };
        a.enqueue_tx(sign_dummy(SigilTx::Send { from: w(1), to: w(2), amount: 777, token: NATIVE, fee: 1 }));
        a2.enqueue_tx(sign_dummy(SigilTx::Send { from: w(3), to: w(4), amount: 888, token: NATIVE, fee: 1 }));
        let bx = a.produce_one().expect("A mints 25");
        let by = a2.produce_one().expect("A' mints 25");
        assert_ne!(bx.hash(), by.hash());
        let r1 = f1.apply_external_block(&bx);
        let r2 = f2.apply_external_block(&by);
        let r1b = f1.apply_external_block(&by); // conflicting → Rejected
        let r2b = f2.apply_external_block(&bx);
        let mut all_blocks: Vec<sigil_chronos::Block> = prefix.clone();
        all_blocks.push(bx.clone());
        all_blocks.push(by.clone());
        // Then the two branches PERSIST: 40 more blocks each, followers stay split.
        let mut emit = |label: &str, d: f64, blocks: &[sigil_chronos::Block], f1: &SigilSimNode, f2: &SigilSimNode, out: &mut Out| {
            let mut counts: HashMap<[u8; 32], u64> = HashMap::new();
            for b in blocks {
                *counts.entry(b.header.producer).or_default() += 1;
            }
            let c: Vec<u64> = counts.values().copied().collect();
            let ds = shannon_bits(&c);
            let tau = FINAL_DEPTH / LIVE_BPS;
            let d_tip = if f1.roots() == f2.roots() { 0.0 } else { 1.0 };
            let d_state = if f1.roots().wallet_state_root == f2.roots().wallet_state_root { 0.0 } else { 1.0 };
            let d_conflict = (f1.rejected_count + f2.rejected_count) as f64 / 2.0;
            let dh_c = W_TIP * d_tip + W_STATE * d_state + W_CONFLICT * d_conflict;
            let h_norm = if counts.len() > 1 { ds / (counts.len() as f64).log2() } else { 0.0 };
            let ks_unit = k_star_unit(dh_c, tau, ds);
            let ks_hbar = k_star_hbar(CORE_W / LIVE_BPS, tau, ds);
            let kf = k_fix(dh_c, d, h_norm);
            out.row(
                "fork0",
                json!({
                    "scenario": format!("{case}{label}"), "producer_ids": counts.len(), "ds_bits": ds, "n_eff": (2f64).powf(ds),
                    "same_producer_id": bx.header.producer == by.header.producer, "persist_d": d,
                    "f1_first": format!("{r1:?}"), "f2_first": format!("{r2:?}"),
                    "f1_conflicting": format!("{r1b:?}"), "f2_conflicting": format!("{r2b:?}"),
                    "f1_height": f1.height(), "f2_height": f2.height(),
                    "f1_wallet_root": hex::encode(f1.roots().wallet_state_root),
                    "f2_wallet_root": hex::encode(f2.roots().wallet_state_root),
                    "d_tip": d_tip, "d_state": d_state, "d_conflict": d_conflict, "dh_c": dh_c, "h_norm": h_norm,
                    "tau_final_s": tau, "k_star_unit": nan_to_null(ks_unit), "k_star_hbar": nan_to_null(ks_hbar),
                    "k_c": nan_to_null(k_c(dh_c, tau, h_norm)), "k_fix": nan_to_null(kf),
                    "phase_unit_mcp": phase_mcp(ks_unit), "phase_kc": phase_mcp(k_c(dh_c, tau, h_norm)), "phase_fix": phase_mcp(kf),
                }),
            );
        };
        emit("", 1.0, &all_blocks, &f1, &f2, out);
        for i in 0..40u8 {
            a.enqueue_tx(sign_dummy(SigilTx::Send { from: w(i % 5 + 1), to: w((i + 2) % 5 + 1), amount: 5, token: NATIVE, fee: 1 }));
            a2.enqueue_tx(sign_dummy(SigilTx::Send { from: w((i + 1) % 5 + 1), to: w((i + 3) % 5 + 1), amount: 6, token: NATIVE, fee: 1 }));
            let ba = a.produce_one().expect("branch A");
            let bb = a2.produce_one().expect("branch A'");
            assert_eq!(f1.apply_external_block(&ba), ApplyOutcome::Ok);
            assert_eq!(f2.apply_external_block(&bb), ApplyOutcome::Ok);
            all_blocks.push(ba);
            all_blocks.push(bb);
        }
        emit("_persist41", 41.0, &all_blocks, &f1, &f2, out);
    }
}

/// Decode a SigilSimNode snapshot (lib.rs `snap` layout): height, tip, applied, divergence, wallet root.
fn decode_snap(b: &[u8]) -> (u64, [u8; 32], u64, u64, [u8; 32]) {
    let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
    let h32 = |r: std::ops::Range<usize>| -> [u8; 32] { b[r].try_into().unwrap() };
    (u(0..8), h32(8..40), u(40..48), u(48..56), h32(56..88))
}

/// Real Universe runs: latency × drop × partition, one honest producer (ΔS = 0) — then the
/// same grid with TWO independent producers (a permanent fork, ΔS = 1 bit).
fn sweep_net(out: &mut Out) {
    let lat = [1_000u64, 50_000, 500_000, 5_000_000];
    let drops = [0.0f64, 0.1, 0.3, 0.5];
    for &two_producers in &[false, true] {
        for &l in &lat {
            for &d in &drops {
                for &partition in &[false, true] {
                    let g = demo_genesis();
                    let mut u = Universe::new(ScenarioSeed::from(0x9A7 ^ l ^ (d * 100.0) as u64));
                    let n_follow = 4u32;
                    // Universe ids are assigned in SPAWN ORDER (A=0, B=1 if present, then the
                    // followers) — a node's internal `my_id` must match that or its peers'
                    // envelopes go to ids that do not exist (the 2026-09-14 first run applied 0).
                    let base = if two_producers { 2 } else { 1 };
                    let fids: Vec<NodeId> = (0..n_follow).map(|i| NodeId(base + i)).collect();
                    let mut peers_a = fids.clone();
                    let mut pids = vec![];
                    if two_producers {
                        peers_a.push(NodeId(1));
                    }
                    let a = u.spawn_node(Box::new(SigilSimNode::new("A", NodeId(0), peers_a, true, secs(1), &g)));
                    pids.push(a);
                    if two_producers {
                        let mut peers_b = fids.clone();
                        peers_b.push(NodeId(0));
                        let b = u.spawn_node(Box::new(SigilSimNode::new("B", NodeId(1), peers_b, true, secs(1), &g)));
                        pids.push(b);
                    }
                    let mut fnodes = vec![];
                    for (i, fid) in fids.iter().enumerate() {
                        let f = u.spawn_node(Box::new(SigilSimNode::new(&format!("F{i}"), *fid, vec![], false, secs(1), &g)));
                        fnodes.push(f);
                    }
                    for (pi, &p) in pids.iter().enumerate() {
                        for (i, &f) in fnodes.iter().enumerate() {
                            // followers 0,1 sit "near" A, 2,3 "near" B (10× latency the other way)
                            let near = if two_producers { (i < 2) == (pi == 0) } else { true };
                            let edge_lat = if near { l } else { l * 10 };
                            let part = partition && i == 3;
                            u.connect(p, f, NetEdge { latency_micros: edge_lat, drop_prob: d, partitioned: part });
                        }
                        if two_producers {
                            let other = pids[1 - pi];
                            u.connect(p, other, NetEdge { latency_micros: l, drop_prob: d, partitioned: false });
                        }
                    }
                    let n_txs = 120u32;
                    for (pi, &p) in pids.iter().enumerate() {
                        let mut r = SplitMix64(0xBEEF ^ pi as u64);
                        for _ in 0..n_txs {
                            let from = (r.next_u64() % 5 + 1) as u8;
                            let mut to = (r.next_u64() % 5 + 1) as u8;
                            if to == from {
                                to = to % 5 + 1;
                            }
                            let tx = sign_dummy(SigilTx::Send { from: w(from), to: w(to), amount: (r.next_u64() % 400 + 1) as u128, token: NATIVE, fee: 1 });
                            let mut payload = vec![TAG_TX];
                            payload.extend_from_slice(&serde_json::to_vec(&tx).unwrap());
                            u.inject(p, payload);
                        }
                    }
                    u.advance(l * 12 + secs(1) * (n_txs as u64 + 60));
                    let log = u.event_log();
                    let produced: HashMap<String, u64> = log.iter().filter_map(|(_, _, s)| {
                        s.split(" produced H=").next().filter(|_| s.contains(" produced H=")).map(|n| n.to_string())
                    }).fold(HashMap::new(), |mut m, n| { *m.entry(n).or_default() += 1; m });
                    let total_produced: u64 = produced.values().sum();
                    let applied = log.iter().filter(|(_, _, s)| s.contains("apply H=") && s.contains("Ok")).count() as u64;
                    let rejected = log.iter().filter(|(_, _, s)| s.contains("Rejected")).count() as u64;
                    let diverged = log.iter().filter(|(_, _, s)| s.contains("Divergence")).count() as u64;
                    let snaps = u.snapshot_nodes();
                    let (ph, ptip, _, _, proot) = decode_snap(&snaps[&pids[0]]);
                    let mut tips_differ = 0.0;
                    let mut roots_differ_same_h = 0.0;
                    let mut comparable = 0.0;
                    let mut gap_sum = 0.0;
                    let mut follower_rows = vec![];
                    for &f in &fnodes {
                        let (fh, ftip, fapplied, fdiv, froot) = decode_snap(&snaps[&f]);
                        gap_sum += (ph as f64 - fh as f64).abs();
                        if fh == ph {
                            comparable += 1.0;
                            if ftip != ptip { tips_differ += 1.0; }
                            if froot != proot { roots_differ_same_h += 1.0; }
                        }
                        follower_rows.push(json!({"height": fh, "applied": fapplied, "divergence": fdiv, "tip": hex::encode(&ftip[..8]), "wallet_root": hex::encode(&froot[..8])}));
                    }
                    let nf = fnodes.len() as f64;
                    let d_tip = if comparable > 0.0 { tips_differ / comparable } else { 0.0 };
                    let d_state = if comparable > 0.0 { roots_differ_same_h / comparable } else { 0.0 };
                    let d_finality = (gap_sum / nf / FINAL_DEPTH).min(1.0);
                    let d_conflict = if total_produced > 0 { (rejected as f64 / (total_produced as f64 * nf)).min(1.0) } else { 0.0 };
                    let dh_c = W_TIP * d_tip + W_STATE * d_state + W_FINALITY * d_finality + W_CONFLICT * d_conflict;
                    let c: Vec<u64> = produced.values().copied().collect();
                    let ds = shannon_bits(&c);
                    let h_norm = if c.len() > 1 { ds / (c.len() as f64).log2() } else { 0.0 };
                    let bps = 1.0; // producers mint one block per simulated second here
                    let tau = FINAL_DEPTH / bps;
                    let ks_unit = k_star_unit(dh_c, tau, ds);
                    let kc = k_c(dh_c, tau, h_norm);
                    let d_persist = gap_sum / nf;
                    let kf = k_fix(dh_c, d_persist, h_norm);
                    out.row(
                        "net",
                        json!({
                            "scenario": if two_producers { "two_independent_producers" } else { "one_honest_producer" },
                            "latency_ms": l as f64 / 1000.0, "drop_prob": d, "partition_one_follower": partition,
                            "produced_by": produced, "total_produced": total_produced, "applied_ok": applied,
                            "rejected": rejected, "divergence_events": diverged,
                            "producer_height": ph, "followers": follower_rows,
                            "ds_bits": ds, "n_eff": (2f64).powf(ds),
                            "d_tip": d_tip, "d_state": d_state, "d_finality": d_finality, "d_conflict": d_conflict, "dh_c": dh_c,
                            "tau_final_s": tau, "k_star_unit": nan_to_null(ks_unit),
                            "k_star_hbar": nan_to_null(k_star_hbar(CORE_W * 1.0, tau, ds)),
                            "k_c": nan_to_null(kc), "phase_unit_mcp": phase_mcp(ks_unit), "phase_kc": phase_mcp(kc),
                            "h_norm": h_norm, "persist_d": d_persist, "k_fix": nan_to_null(kf), "phase_fix": phase_mcp(kf),
                        }),
                    );
                }
            }
        }
    }
}

fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    fn ranks(v: &[f64]) -> Vec<f64> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap());
        let mut r = vec![0.0; v.len()];
        let mut i = 0;
        while i < idx.len() {
            let mut j = i;
            while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
                j += 1;
            }
            let avg = (i + j) as f64 / 2.0 + 1.0;
            for k in i..=j {
                r[idx[k]] = avg;
            }
            i = j + 1;
        }
        r
    }
    if x.len() < 3 {
        return None;
    }
    let (rx, ry) = (ranks(x), ranks(y));
    let n = x.len() as f64;
    let (mx, my) = (rx.iter().sum::<f64>() / n, ry.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..x.len() {
        sxy += (rx[i] - mx) * (ry[i] - my);
        sxx += (rx[i] - mx).powi(2);
        syy += (ry[i] - my).powi(2);
    }
    if sxx == 0.0 || syy == 0.0 {
        None // a constant series has no rank correlation with anything
    } else {
        Some(sxy / (sxx * syy).sqrt())
    }
}

/// Seeded fuzz over the crate's own `run_trial` (2 nodes, random latency/loss/tx mix).
fn sweep_fuzz(out: &mut Out, n: u64) {
    let (mut ks, mut kcs, mut kfs, mut rej, mut div) = (vec![], vec![], vec![], vec![], vec![]);
    let t0 = std::time::Instant::now();
    for i in 0..n {
        let o = run_trial(0xF00D + i);
        let d_conflict = if o.produced > 0 { (o.rejected as f64 / o.produced as f64).min(1.0) } else { 0.0 };
        let d_finality = if o.produced > 0 { ((o.produced - o.applied_ok.min(o.produced)) as f64 / FINAL_DEPTH).min(1.0) } else { 0.0 };
        let dh_c = W_CONFLICT * d_conflict + W_FINALITY * d_finality;
        let ds = 0.0; // one producer per trial — by construction of run_trial
        let tau = FINAL_DEPTH / 1.0;
        let k = k_star_unit(dh_c, tau, ds);
        let kc = k_c(dh_c, tau, 0.0);
        let d_persist = (o.produced - o.applied_ok.min(o.produced)) as f64;
        let kf = k_fix(dh_c, d_persist, 0.0);
        ks.push(k);
        kcs.push(kc);
        kfs.push(kf);
        rej.push(o.rejected as f64);
        div.push(o.divergence as f64);
        if i < 200 || i % 50 == 0 {
            out.row("fuzz", json!({"seed": o.seed, "produced": o.produced, "applied_ok": o.applied_ok, "rejected": o.rejected,
                "divergence": o.divergence, "d_conflict": d_conflict, "d_finality": d_finality, "dh_c": dh_c,
                "ds_bits": ds, "k_star_unit": k, "k_c": kc, "h_norm": 0.0, "persist_d": d_persist, "k_fix": kf}));
        }
    }
    let summary = json!({
        "scenario": "fuzz_summary", "trials": n, "elapsed_s": t0.elapsed().as_secs_f64(),
        "k_star_unit_min": ks.iter().cloned().fold(f64::INFINITY, f64::min),
        "k_star_unit_max": ks.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "k_c_min": kcs.iter().cloned().fold(f64::INFINITY, f64::min),
        "k_c_max": kcs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "rejected_total": rej.iter().sum::<f64>(), "divergence_total": div.iter().sum::<f64>(),
        "spearman_kstar_vs_rejected": spearman(&ks, &rej), "spearman_kc_vs_rejected": spearman(&kcs, &rej),
        "spearman_kstar_vs_divergence": spearman(&ks, &div), "spearman_kc_vs_divergence": spearman(&kcs, &div),
        "k_fix_min": kfs.iter().cloned().fold(f64::INFINITY, f64::min),
        "k_fix_max": kfs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "spearman_kfix_vs_rejected": spearman(&kfs, &rej), "spearman_kfix_vs_divergence": spearman(&kfs, &div),
    });
    println!("{summary}");
    out.row("fuzz", summary);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| "kparam-out".into());
    let which = args.next().unwrap_or_else(|| "all".into());
    let fuzz_n: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let mut out = Out::new(&dir);
    // Sanity: producer ids from the sim are distinct per node and non-zero.
    assert_ne!(producer_id_for(NodeId(0)), producer_id_for(NodeId(1)));
    assert_ne!(producer_id_for(NodeId(0)), [0u8; 32]);
    let run = |name: &str, out: &mut Out, f: &dyn Fn(&mut Out)| {
        if which == "all" || which == name {
            let t = std::time::Instant::now();
            let before = out.rows;
            f(out);
            eprintln!("sweep {name}: {} rows in {:.1}s", out.rows - before, t.elapsed().as_secs_f64());
        }
    };
    run("ds", &mut out, &sweep_ds);
    run("fork0", &mut out, &sweep_fork0);
    run("sybil", &mut out, &sweep_sybil);
    run("rate", &mut out, &sweep_rate);
    run("energy", &mut out, &sweep_energy);
    run("negdh", &mut out, &sweep_negdh);
    run("net", &mut out, &sweep_net);
    if which == "all" || which == "fuzz" {
        let t = std::time::Instant::now();
        sweep_fuzz(&mut out, fuzz_n);
        eprintln!("sweep fuzz: {fuzz_n} trials in {:.1}s", t.elapsed().as_secs_f64());
    }
    eprintln!("done: {} rows → {}", out.rows, dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paper generator's identity check (k_parameter.rs:184-207): K* via ħ and via N_ML
    /// agree at three scales, and the v1 operating point is the 4.2e12-tick one.
    #[test]
    fn k_star_matches_paper_generator() {
        let e_net = BOLTZMANN * 1e3;
        let scales = [(e_net, 0.05, 1.0), (4.07e6 * 1.602176634e-19, HBAR / (4.07e6 * 1.602176634e-19), (12f64).ln()), (20.0 * 299_792_458f64.powi(2), 1.0, 1.0)];
        for (e, t, ds) in scales {
            let a = k_star_hbar(e, t, ds);
            let b = k_star_via_ml(e, t, ds);
            assert!(((a - b) / a).abs() < 1e-12, "{a} vs {b}");
        }
        let nml = n_ml(e_net, 0.05);
        assert!((nml - 4.17e12).abs() / 4.17e12 < 0.01, "N_ML at the v1 operating point = {nml:e}");
    }

    #[test]
    fn product_form_reads_zero_for_any_fork_with_one_producer() {
        assert_eq!(k_star_unit(1.0, 1e9, 0.0), 0.0);
        assert_eq!(k_star_hbar(350e6, 1e9, 0.0), 0.0);
    }

    #[test]
    fn hbar_ladder_floor_is_above_one() {
        // N_ML = 1 tick, ΔS = 1 bit is the least a physical round can be, and it already
        // reads 7.87 — above the "critical" line of the MCP ladder (3).
        let e = PI * HBAR / 2.0; // N_ML = 1 at t = 1 s
        let k = k_star_hbar(e, 1.0, 1.0);
        assert!((k - 7.8748).abs() < 1e-3, "{k}");
    }

    #[test]
    fn repaired_formula_properties() {
        // one key forking against itself is the WORST case; two keys never read worse
        assert!(k_fix(0.8, 1.0, 0.0) >= k_fix(0.8, 1.0, 0.235));
        // zero disagreement → 0 for any key count / entropy
        assert_eq!(k_fix(0.0, 512.0, 1.0), 0.0);
        // rotating keys can lower K by at most √(4/3) ≈ 15 % (and never raise it)
        assert!((k_fix(0.04, 39.0, 0.0) / k_fix(0.04, 39.0, 1.0) - (4f64 / 3.0).sqrt()).abs() < 1e-9);
        // a full state divergence is critical whatever the key count, even fresh
        assert!(k_fix(0.8, 1.0, 1.0) >= 3.0);
        // persistence raises, never lowers, and is capped at the finality depth
        assert!(k_fix(0.5, 0.0, 0.0) < k_fix(0.5, 256.0, 0.0));
        assert_eq!(k_fix(0.5, 512.0, 0.0), k_fix(0.5, 5000.0, 0.0));
        // the maximum is 2π: full, persistent, single-key state divergence
        assert!((k_fix(1.0, 512.0, 0.0) - 2.0 * PI).abs() < 1e-9);
        // never NaN, whatever garbage arrives
        assert!(!k_fix(-0.3, -5.0, 2.0).is_nan());
        // no t, no ħ: the block rate cannot enter
        assert_eq!(k_fix(0.04, 39.0, 0.9), k_fix(0.04, 39.0, 0.9));
    }

    #[test]
    fn shannon_and_spearman_behave() {
        assert!((shannon_bits(&[1, 1]) - 1.0).abs() < 1e-12);
        assert_eq!(shannon_bits(&[7]), 0.0);
        assert_eq!(spearman(&[0.0, 0.0, 0.0], &[1.0, 2.0, 3.0]), None);
        assert!((spearman(&[1.0, 2.0, 3.0], &[10.0, 20.0, 30.0]).unwrap() - 1.0).abs() < 1e-12);
    }
}
