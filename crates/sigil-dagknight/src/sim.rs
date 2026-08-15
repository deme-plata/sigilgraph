//! The VARFLOW sim gate — deterministic adversarial simulation of the braid
//! ordering core (`docs/SIGIL_DAGKNIGHT_LANE_v0.md` §4).
//!
//! Six pure scenarios (S1–S6) exercise the **deterministic braid
//! linearization** (NOT GHOSTDAG, NOT DagKnight-the-paper — see the crate
//! header) against a faithful miniature of the live dag_mode topology:
//! `P` producers each minting their own spine (empty transitions), seeded
//! cross-`merge_parents` from the other producers' tips, and a seeded
//! delay/drop/reorder delivery schedule per node. Everything is driven by a
//! seeded xorshift64 RNG — no `rand`, no clocks, no `SystemTime` — so every
//! run is bit-reproducible from its `u64` seed.
//!
//! Pattern: Report-struct + pure `run_*` functions (the sigil-chronos
//! turbosync shape, replicated NOT imported — that crate is another agent's
//! lane).

use std::collections::{HashMap, HashSet};

use sigil_header::BlockHash;

use crate::view::BlockView;
use crate::{Braid, BraidConfig, InsertOutcome};

// ─── deterministic RNG ──────────────────────────────────────────────────────

/// xorshift64 — tiny, seeded, fully deterministic. Zero seeds are bumped to 1.
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.max(1),
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform-ish in `0..n` (`n` > 0; modulo bias irrelevant here).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ─── small helpers ──────────────────────────────────────────────────────────

fn hex32(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Chained BLAKE3 over a hash slice — replicates the braid's `order_hash`
/// chaining so scenarios can hash arbitrary prefixes independently.
fn chain_over(hashes: &[BlockHash]) -> [u8; 32] {
    let mut acc = [0u8; 32];
    for h in hashes {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&acc);
        hasher.update(h);
        acc = *hasher.finalize().as_bytes();
    }
    acc
}

/// Failure collector: scenarios record every violated criterion and pass only
/// if none were recorded.
#[derive(Default)]
struct Checks {
    fails: Vec<String>,
}

impl Checks {
    fn ok(&mut self, cond: bool, what: impl Into<String>) {
        if !cond {
            self.fails.push(what.into());
        }
    }

    fn passed(&self) -> bool {
        self.fails.is_empty()
    }

    fn detail(&self, ok_msg: String) -> String {
        if self.fails.is_empty() {
            ok_msg
        } else {
            format!("FAILED: {}", self.fails.join("; "))
        }
    }
}

/// Positions where two linearizations differ (plus any length difference).
fn divergence_count(a: &[BlockHash], b: &[BlockHash]) -> u64 {
    let common = a.len().min(b.len());
    let mut d = (a.len().max(b.len()) - common) as u64;
    for i in 0..common {
        if a[i] != b[i] {
            d += 1;
        }
    }
    d
}

fn shuffle(rng: &mut XorShift64, v: &mut [usize]) {
    for i in (1..v.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        v.swap(i, j);
    }
}

// ─── DAG generator (live dag_mode topology in miniature) ────────────────────

const ATTACKER: [u8; 32] = [0xEE; 32];

fn producer_id(i: u8) -> [u8; 32] {
    [i.wrapping_add(1); 32]
}

fn mk_hash(producer: &[u8; 32], height: u64, parent: &BlockHash, nonce: u64) -> BlockHash {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-dagknight/sim/v0");
    h.update(producer);
    h.update(&height.to_le_bytes());
    h.update(parent);
    h.update(&nonce.to_le_bytes());
    *h.finalize().as_bytes()
}

/// A generated braid: views in creation (topologically valid) order, genesis
/// first, plus lookup indexes the delivery/backfill harnesses need.
struct GenBraid {
    views: Vec<BlockView>,
    by_hash: HashMap<BlockHash, usize>,
    max_height: u64,
    /// The equivocating pair, ordered (smaller hash, larger hash).
    equivocation: Option<(BlockHash, BlockHash)>,
}

/// `P` producers each mint on their own spine, cross-referencing the other
/// producers' current tips as `merge_parents` on a seeded schedule. Producer
/// heights are coupled (spread ≤ 16) — live same-rate producers — so honest
/// generation never strays outside a 64-deep finality window. `equivocate`
/// mints one same-producer/same-height/same-parent twin mid-run.
fn gen_braid(rng: &mut XorShift64, producers: u8, blocks: u64, equivocate: bool) -> GenBraid {
    let producers = producers.max(1) as usize;
    let mut views: Vec<BlockView> = Vec::with_capacity(blocks as usize + 1);
    let genesis_producer = producer_id(0);
    let genesis_hash = mk_hash(&genesis_producer, 0, &[0u8; 32], 0);
    views.push(BlockView {
        hash: genesis_hash,
        parent: [0u8; 32],
        merge_parents: Vec::new(),
        height: 0,
        producer: genesis_producer,
    });
    let mut tips: Vec<(BlockHash, u64)> = vec![(genesis_hash, 0); producers];
    let mut nonce = 1u64;
    let equiv_at = if equivocate { (blocks / 2).max(2) } else { u64::MAX };
    let mut equivocation: Option<(BlockHash, BlockHash)> = None;

    while (views.len() as u64) < blocks {
        // Height coupling: if the spread exceeds 16, the laggard mints next.
        let min_h = tips.iter().map(|t| t.1).min().unwrap_or(0);
        let max_h = tips.iter().map(|t| t.1).max().unwrap_or(0);
        let p = if max_h - min_h > 16 {
            tips.iter().position(|t| t.1 == min_h).unwrap_or(0)
        } else {
            rng.below(producers as u64) as usize
        };
        let (parent, ph) = tips[p];
        let height = ph + 1;
        let mut merges: Vec<BlockHash> = Vec::new();
        for (q, &(qt, _)) in tips.iter().enumerate() {
            if q == p || qt == parent || merges.contains(&qt) || merges.len() >= 3 {
                continue;
            }
            if rng.below(100) < 50 {
                merges.push(qt);
            }
        }
        let pid = producer_id(p as u8);
        let hash = mk_hash(&pid, height, &parent, nonce);
        nonce += 1;
        views.push(BlockView {
            hash,
            parent,
            merge_parents: merges,
            height,
            producer: pid,
        });
        tips[p] = (hash, height);

        if views.len() as u64 == equiv_at && equivocation.is_none() {
            // Equivocation twin: same producer, height, spine parent —
            // different hash (nonce). Both are structurally valid.
            let twin = mk_hash(&pid, height, &parent, nonce);
            nonce += 1;
            views.push(BlockView {
                hash: twin,
                parent,
                merge_parents: Vec::new(),
                height,
                producer: pid,
            });
            equivocation = Some(if hash < twin { (hash, twin) } else { (twin, hash) });
        }
    }

    let max_height = views.iter().map(|v| v.height).max().unwrap_or(0);
    let by_hash = views.iter().enumerate().map(|(i, v)| (v.hash, i)).collect();
    GenBraid {
        views,
        by_hash,
        max_height,
        equivocation,
    }
}

/// Config for permuted feeds: window/pending big enough for the whole DAG,
/// `final_depth` high enough that arbitrary arrival orders never trip the
/// `BelowFinal` guard (that guard is what S3 tests on purpose).
fn open_cfg(n: usize, final_depth: u64) -> BraidConfig {
    BraidConfig {
        final_depth,
        max_window: n + 64,
        max_pending: n + 64,
        max_merge_parents: 4,
        ghostdag_k: None,
        final_blue_depth: None,
    }
}

// ─── the report ─────────────────────────────────────────────────────────────

/// Result of one sim scenario — the turbosync-report shape: public fields plus
/// a one-line `summary()`.
#[derive(Debug, Clone)]
pub struct BraidSimReport {
    /// Scenario tag, e.g. `"S1 dual-instance"`.
    pub scenario: &'static str,
    /// Blocks in the generated DAG (attacker/extension blocks excluded).
    pub blocks: u64,
    /// Producers minting the honest braid.
    pub producers: u8,
    /// Scenario-specific divergence count — 0 on pass (mismatched
    /// linearization positions, disagreeing permutations, perturbed-prefix
    /// events, or window-cap violations, per scenario).
    pub divergence: u64,
    /// Hex of the scenario's headline order hash (full order or finalized
    /// prefix, per scenario) — the cross-run reproducibility anchor.
    pub order_hash_hex: String,
    /// All pass criteria held.
    pub passed: bool,
    /// Headline numbers on pass; joined failure list on fail.
    pub detail: String,
}

impl BraidSimReport {
    /// One-line summary for the gate binary.
    pub fn summary(&self) -> String {
        let oh_short: String = self.order_hash_hex.chars().take(16).collect();
        format!(
            "{scenario:<24} · {blocks:>6} blocks · {producers} producers · divergence {div} · order_hash {oh}… · {verdict} — {detail}",
            scenario = self.scenario,
            blocks = self.blocks,
            producers = self.producers,
            div = self.divergence,
            oh = oh_short,
            verdict = if self.passed { "PASS" } else { "FAIL" },
            detail = self.detail,
        )
    }
}

// ─── S1: dual-instance agreement ────────────────────────────────────────────

/// S1 — the same generated DAG fed to two independent [`Braid`] instances in
/// two different arrival orders (creation order vs a full seeded shuffle)
/// must produce identical `linearize()` vectors and equal `order_hash`.
pub fn run_dual_instance(seed: u64, producers: u8, blocks: u64) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let n = gen.views.len();
    let cfg = open_cfg(n, blocks + 1);
    let mut checks = Checks::default();

    let mut a = Braid::new(cfg.clone());
    let mut bad_a = 0u64;
    for v in &gen.views {
        if !matches!(a.insert(v.clone()), InsertOutcome::Inserted { .. }) {
            bad_a += 1;
        }
    }
    checks.ok(bad_a == 0, format!("A: {bad_a} creation-order inserts not Inserted"));

    let mut order_b: Vec<usize> = (0..n).collect();
    shuffle(&mut rng, &mut order_b);
    let mut b = Braid::new(cfg);
    let mut bad_b = 0u64;
    for &i in &order_b {
        match b.insert(gen.views[i].clone()) {
            InsertOutcome::Inserted { .. } | InsertOutcome::MissingParents(_) => {}
            _ => bad_b += 1,
        }
    }
    checks.ok(bad_b == 0, format!("B: {bad_b} shuffled inserts rejected"));
    checks.ok(
        b.missing_parents().is_empty(),
        "B: unresolved missing parents after full delivery",
    );

    let lin_a = a.linearize();
    let lin_b = b.linearize();
    let div = divergence_count(&lin_a, &lin_b);
    checks.ok(lin_a.len() == n, "A: linearization does not cover the DAG");
    checks.ok(div == 0, format!("linearizations diverge at {div} positions"));
    let oh_a = a.order_hash();
    checks.ok(oh_a == b.order_hash(), "order_hash mismatch between instances");

    let passed = checks.passed();
    BraidSimReport {
        scenario: "S1 dual-instance",
        blocks: n as u64,
        producers,
        divergence: div,
        order_hash_hex: hex32(&oh_a),
        passed,
        detail: checks.detail(format!(
            "2 instances · 2 arrival orders (creation vs full shuffle) · {n} blocks linearized identically"
        )),
    }
}

// ─── S7: GHOSTDAG (v2) dual-instance agreement ──────────────────────────────

/// S7 — the v2 analog of S1: the same generated DAG fed to two independent
/// [`Braid`] instances (both with `ghostdag_k = Some(k)`) in two different
/// arrival orders must agree on `linearize()`, `order_hash()`, the selected
/// tip, AND — the v2-specific check S1 has no equivalent of — every block's
/// `blue_score()`. v1's linearization is untouched by the ghostdag lane, so
/// this scenario exists specifically to prove the NEW coloring/scoring is
/// itself a pure function of the DAG, not of arrival order, at realistic
/// scale (matches S1's scale: 3 producers, 10,000 blocks).
pub fn run_ghostdag_dual_instance(seed: u64, producers: u8, blocks: u64, k: u32) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let n = gen.views.len();
    let mut cfg = open_cfg(n, blocks + 1);
    cfg.ghostdag_k = Some(k);
    let mut checks = Checks::default();

    let mut a = Braid::new(cfg.clone());
    let mut bad_a = 0u64;
    for v in &gen.views {
        if !matches!(a.insert(v.clone()), InsertOutcome::Inserted { .. }) {
            bad_a += 1;
        }
    }
    checks.ok(bad_a == 0, format!("A: {bad_a} creation-order inserts not Inserted"));
    checks.ok(a.is_ghostdag_active(), "A: ghostdag lane not active");

    let mut order_b: Vec<usize> = (0..n).collect();
    shuffle(&mut rng, &mut order_b);
    let mut b = Braid::new(cfg);
    let mut bad_b = 0u64;
    for &i in &order_b {
        match b.insert(gen.views[i].clone()) {
            InsertOutcome::Inserted { .. } | InsertOutcome::MissingParents(_) => {}
            _ => bad_b += 1,
        }
    }
    checks.ok(bad_b == 0, format!("B: {bad_b} shuffled inserts rejected"));
    checks.ok(
        b.missing_parents().is_empty(),
        "B: unresolved missing parents after full delivery",
    );

    let lin_a = a.linearize();
    let lin_b = b.linearize();
    let div = divergence_count(&lin_a, &lin_b);
    checks.ok(lin_a.len() == n, "A: linearization does not cover the DAG");
    checks.ok(div == 0, format!("linearizations diverge at {div} positions"));
    let oh_a = a.order_hash();
    checks.ok(oh_a == b.order_hash(), "order_hash mismatch between instances");
    checks.ok(
        a.selected_tip() == b.selected_tip(),
        "selected tip (blue-score based) mismatch between instances",
    );

    // The v2-specific check: blue_score is a pure function of the DAG.
    let mut blue_score_mismatches = 0u64;
    for v in &gen.views {
        if a.blue_score(&v.hash) != b.blue_score(&v.hash) {
            blue_score_mismatches += 1;
        }
    }
    checks.ok(
        blue_score_mismatches == 0,
        format!("{blue_score_mismatches} blocks have divergent blue_score across arrival orders"),
    );

    let passed = checks.passed();
    BraidSimReport {
        scenario: "S7 ghostdag dual-instance",
        blocks: n as u64,
        producers,
        divergence: div + blue_score_mismatches,
        order_hash_hex: hex32(&oh_a),
        passed,
        detail: checks.detail(format!(
            "k={k} · 2 instances · 2 arrival orders · {n} blocks · linearization + blue_score fully agree"
        )),
    }
}

// ─── S2: permutation invariance + incremental==batch ────────────────────────

/// S2 — `perms` seeded arrival permutations of one DAG must all agree on
/// `order_hash` (and the full `linearize()` vector). Per permutation, the
/// concatenation of incremental `drain_ordered()` calls must equal the
/// finalized prefix of the batch `linearize()`, and — after a deterministic
/// spine extension pushes finality past the whole DAG — every original block
/// must appear in the drained (finalized) order.
pub fn run_permutation_invariance(seed: u64, perms: u32) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let producers = 3u8;
    let blocks = 2_000u64;
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let n = gen.views.len();
    let final_depth = gen.max_height + 1;
    let ext_len = final_depth + 2;
    let cfg = BraidConfig {
        final_depth,
        max_window: n + ext_len as usize + 64,
        max_pending: n + 64,
        max_merge_parents: 4,
        ghostdag_k: None,
        final_blue_depth: None,
    };
    let mut checks = Checks::default();

    // Selected tip of the generated DAG (max height, min-hash tie-break) is a
    // pure function of the DAG, so the finalizing extension — a linear chain
    // from it — is minted once and shared by every permutation.
    let tip = gen
        .views
        .iter()
        .max_by(|x, y| x.height.cmp(&y.height).then(y.hash.cmp(&x.hash)))
        .expect("non-empty DAG");
    let mut ext: Vec<BlockView> = Vec::with_capacity(ext_len as usize);
    let (mut ep, mut eh) = (tip.hash, tip.height);
    let mut nonce = 5_000_000u64;
    for _ in 0..ext_len {
        let hash = mk_hash(&producer_id(0), eh + 1, &ep, nonce);
        nonce += 1;
        ext.push(BlockView {
            hash,
            parent: ep,
            merge_parents: Vec::new(),
            height: eh + 1,
            producer: producer_id(0),
        });
        ep = hash;
        eh += 1;
    }

    let mut first: Option<(Vec<BlockHash>, [u8; 32])> = None;
    let mut bad_perms = 0u64;
    for k in 0..perms {
        let mut order: Vec<usize> = (0..n).collect();
        if k > 0 {
            shuffle(&mut rng, &mut order);
        }
        let mut b = Braid::new(cfg.clone());
        let mut drains: Vec<BlockHash> = Vec::new();
        let mut bad = 0u64;
        for &i in &order {
            match b.insert(gen.views[i].clone()) {
                InsertOutcome::Inserted { .. } | InsertOutcome::MissingParents(_) => {}
                _ => bad += 1,
            }
            drains.extend(b.drain_ordered());
        }
        checks.ok(bad == 0, format!("perm {k}: {bad} inserts rejected"));
        checks.ok(
            b.missing_parents().is_empty(),
            format!("perm {k}: unresolved missing parents"),
        );
        for v in &ext {
            if !matches!(b.insert(v.clone()), InsertOutcome::Inserted { .. }) {
                bad += 1;
            }
            drains.extend(b.drain_ordered());
        }
        checks.ok(bad == 0, format!("perm {k}: extension insert rejected"));

        let lin = b.linearize();
        checks.ok(
            drains.len() <= lin.len() && drains.as_slice() == &lin[..drains.len()],
            format!("perm {k}: concat(drain_ordered) != linearize prefix"),
        );
        let drained: HashSet<&BlockHash> = drains.iter().collect();
        checks.ok(
            gen.views.iter().all(|v| drained.contains(&v.hash)),
            format!("perm {k}: not every original block finalized"),
        );
        let oh = b.order_hash();
        match &first {
            None => first = Some((lin, oh)),
            Some((l0, o0)) => {
                if *o0 != oh || *l0 != lin {
                    bad_perms += 1;
                }
            }
        }
    }
    checks.ok(bad_perms == 0, format!("{bad_perms} permutations disagree"));

    let oh = first.map(|(_, o)| o).unwrap_or([0u8; 32]);
    let passed = checks.passed();
    BraidSimReport {
        scenario: "S2 permutation-invariance",
        blocks: n as u64,
        producers,
        divergence: bad_perms,
        order_hash_hex: hex32(&oh),
        passed,
        detail: checks.detail(format!(
            "{perms}/{perms} permutations agree · incremental==batch each · all {n} blocks finalized"
        )),
    }
}

// ─── S3: withheld attacker chain ────────────────────────────────────────────

/// S3 — an honest DAG grows past finality, then an attacker releases (a) a
/// private fork branching *below* `finalized_height` — every insert must be
/// `BelowFinal` and the finalized-prefix order hash must stay byte-identical —
/// and (b) a fork *inside* the window — accepted, but only the non-final
/// suffix may reorder; the prefix through `finalized_height` is unchanged.
pub fn run_withheld_attacker(seed: u64, fork_depth: u64) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let producers = 2u8;
    let blocks = 1_500u64;
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let cfg = BraidConfig {
        final_depth: 64,
        ..BraidConfig::default()
    };
    let mut checks = Checks::default();

    let mut b = Braid::new(cfg);
    let mut drains: Vec<BlockHash> = Vec::new();
    let mut bad = 0u64;
    for v in &gen.views {
        if !matches!(b.insert(v.clone()), InsertOutcome::Inserted { .. }) {
            bad += 1;
        }
        drains.extend(b.drain_ordered());
    }
    checks.ok(bad == 0, format!("{bad} honest inserts rejected"));

    let f = b.finalized_height();
    checks.ok(
        f > fork_depth + 1,
        format!("finality did not advance enough (f={f}, fork_depth={fork_depth})"),
    );
    let k = drains.len();
    checks.ok(k > 0, "nothing finalized before the attack");
    let prefix = drains.clone();
    let prefix_hash = chain_over(&prefix);
    let lin_before = b.linearize();
    checks.ok(
        lin_before.len() >= k && &lin_before[..k] == prefix.as_slice(),
        "pre-attack prefix mismatch",
    );
    let mut prefix_violations = 0u64;

    // (a) private fork branching below the finalized height.
    let base_h = f.saturating_sub(fork_depth);
    let base_a = gen.views.iter().find(|v| v.height == base_h);
    checks.ok(base_a.is_some(), "no honest block at the below-final branch height");
    let mut nonce = 9_000_000u64;
    let mut not_refused = 0u64;
    if let Some(base) = base_a {
        let mut parent = base.hash;
        for d in 1..=fork_depth {
            let height = base_h + d; // ≤ f by construction
            let hash = mk_hash(&ATTACKER, height, &parent, nonce);
            nonce += 1;
            let out = b.insert(BlockView {
                hash,
                parent,
                merge_parents: Vec::new(),
                height,
                producer: ATTACKER,
            });
            if !matches!(out, InsertOutcome::BelowFinal { .. }) {
                not_refused += 1;
            }
            parent = hash;
        }
    }
    checks.ok(
        not_refused == 0,
        format!("(a) {not_refused} below-final fork blocks not refused"),
    );
    checks.ok(
        b.drain_ordered().is_empty(),
        "(a) drain produced blocks after a refused fork",
    );
    let lin_a = b.linearize();
    let prefix_a_ok = lin_a.len() >= k && &lin_a[..k] == prefix.as_slice();
    if !prefix_a_ok {
        prefix_violations += 1;
    }
    checks.ok(prefix_a_ok, "(a) finalized prefix perturbed");
    checks.ok(
        chain_over(&lin_a[..k.min(lin_a.len())]) == prefix_hash,
        "(a) finalized-prefix order_hash not byte-identical",
    );

    // (b) fork inside the window (above f, below the honest tip).
    let fb = f + 5;
    let fork_len = 10u64;
    let base_b = gen.views.iter().find(|v| v.height == fb);
    checks.ok(base_b.is_some(), "no honest block at the in-window branch height");
    let mut not_accepted = 0u64;
    if let Some(base) = base_b {
        let mut parent = base.hash;
        for d in 1..=fork_len {
            let height = fb + d; // stays < honest tip (f + 64)
            let hash = mk_hash(&ATTACKER, height, &parent, nonce);
            nonce += 1;
            let out = b.insert(BlockView {
                hash,
                parent,
                merge_parents: Vec::new(),
                height,
                producer: ATTACKER,
            });
            if !matches!(out, InsertOutcome::Inserted { .. }) {
                not_accepted += 1;
            }
            parent = hash;
        }
    }
    checks.ok(
        not_accepted == 0,
        format!("(b) {not_accepted} in-window fork blocks not accepted"),
    );
    checks.ok(b.finalized_height() == f, "(b) finality moved under attack");
    checks.ok(
        b.drain_ordered().is_empty(),
        "(b) drain produced blocks without honest growth",
    );
    let lin_b = b.linearize();
    let prefix_b_ok = lin_b.len() >= k && &lin_b[..k] == prefix.as_slice();
    if !prefix_b_ok {
        prefix_violations += 1;
    }
    checks.ok(prefix_b_ok, "(b) finalized prefix perturbed");
    let first_div = lin_before
        .iter()
        .zip(lin_b.iter())
        .position(|(x, y)| x != y)
        .unwrap_or(lin_before.len().min(lin_b.len()));
    checks.ok(
        first_div >= k,
        format!("(b) reorder crossed the finality line (first divergence {first_div} < finalized {k})"),
    );

    let passed = checks.passed();
    BraidSimReport {
        scenario: "S3 withheld-attacker",
        blocks: gen.views.len() as u64,
        producers,
        divergence: prefix_violations,
        order_hash_hex: hex32(&prefix_hash),
        passed,
        detail: checks.detail(format!(
            "fork_depth {fork_depth} below final: all BelowFinal · in-window fork {fork_len}: ordered after prefix ({k} finalized, f={f}) · prefix byte-identical"
        )),
    }
}

// ─── S4: tamper reject ──────────────────────────────────────────────────────

/// S4 — malformed and foreign inputs: an unknown-parent block parks forever
/// and is never ordered; structurally invalid blocks (self-parent, duplicate
/// merge parent, merge==spine parent, >4 merge parents, bad height) are
/// `Rejected`; a duplicate insert is `Duplicate`. In every case the order is
/// unperturbed and nothing panics.
pub fn run_tamper_reject(seed: u64) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let producers = 2u8;
    let blocks = 120u64;
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let n = gen.views.len();
    let cfg = open_cfg(n, blocks + 1);
    let mut checks = Checks::default();

    let mut b = Braid::new(cfg);
    for v in &gen.views {
        b.insert(v.clone());
    }
    let lin0 = b.linearize();
    let oh0 = b.order_hash();
    checks.ok(lin0.len() == n, "honest DAG not fully linearized");

    // 1. Unknown parent → parked, never ordered.
    let ghost_parent = mk_hash(&ATTACKER, 999, &[7u8; 32], 424_242);
    let orphan = mk_hash(&ATTACKER, 50, &ghost_parent, 424_243);
    let out = b.insert(BlockView {
        hash: orphan,
        parent: ghost_parent,
        merge_parents: Vec::new(),
        height: 50,
        producer: ATTACKER,
    });
    checks.ok(
        out == InsertOutcome::MissingParents(vec![ghost_parent]),
        format!("unknown parent not parked: {out:?}"),
    );
    checks.ok(
        b.missing_parents().contains(&ghost_parent),
        "unknown parent absent from the backfill worklist",
    );
    checks.ok(
        !b.linearize().contains(&orphan),
        "parked orphan leaked into the order",
    );

    // 2. Structurally malformed → Rejected (structured, no panic).
    let anchor = gen.views[5].clone();
    let fresh = |i: u64| mk_hash(&ATTACKER, 7, &[9u8; 32], 700 + i);
    let self_parent = mk_hash(&ATTACKER, 3, &[1u8; 32], 1);
    let malformed: Vec<(&'static str, BlockView)> = vec![
        (
            "self-parent",
            BlockView {
                hash: self_parent,
                parent: self_parent,
                merge_parents: Vec::new(),
                height: 3,
                producer: ATTACKER,
            },
        ),
        (
            "too many merge parents",
            BlockView {
                hash: fresh(1),
                parent: anchor.hash,
                merge_parents: (2..7).map(fresh).collect(),
                height: anchor.height + 1,
                producer: ATTACKER,
            },
        ),
        (
            "duplicate merge parent",
            BlockView {
                hash: fresh(10),
                parent: anchor.hash,
                merge_parents: vec![fresh(11), fresh(11)],
                height: anchor.height + 1,
                producer: ATTACKER,
            },
        ),
        (
            "merge parent duplicates spine parent",
            BlockView {
                hash: fresh(20),
                parent: anchor.hash,
                merge_parents: vec![anchor.hash],
                height: anchor.height + 1,
                producer: ATTACKER,
            },
        ),
        (
            "height not parent height + 1",
            BlockView {
                hash: fresh(30),
                parent: anchor.hash,
                merge_parents: Vec::new(),
                height: anchor.height + 7,
                producer: ATTACKER,
            },
        ),
    ];
    let mut rejects = 0u64;
    for (name, view) in &malformed {
        match b.insert(view.clone()) {
            InsertOutcome::Rejected(_) => rejects += 1,
            other => checks.ok(false, format!("{name}: expected Rejected, got {other:?}")),
        }
    }
    checks.ok(
        rejects == malformed.len() as u64,
        format!("only {rejects}/{} malformed inputs rejected", malformed.len()),
    );

    // 3. Duplicate → Duplicate, order unchanged.
    checks.ok(
        b.insert(gen.views[10].clone()) == InsertOutcome::Duplicate,
        "duplicate insert not reported as Duplicate",
    );
    checks.ok(b.linearize() == lin0, "order perturbed by rejected/duplicate inputs");
    checks.ok(b.order_hash() == oh0, "order_hash perturbed by rejected/duplicate inputs");

    let passed = checks.passed();
    BraidSimReport {
        scenario: "S4 tamper-reject",
        blocks: n as u64,
        producers,
        divergence: 0,
        order_hash_hex: hex32(&oh0),
        passed,
        detail: checks.detail(format!(
            "orphan parked (never ordered) · {rejects}/5 malformed → Rejected · duplicate → Duplicate · order untouched"
        )),
    }
}

// ─── S5: live-topology replica (ordering leg) ───────────────────────────────

/// S5a — a faithful replica of the live 2-producer dag_mode topology (empty
/// transitions): seeded gossip drop (`drop_pct`%), bounded delayed/reordered
/// delivery, and optionally an equivocating producer (two blocks, same height,
/// same producer, same parent). Two nodes with independent delivery schedules
/// must converge on the same linearization (dropped blocks park → the
/// `missing_parents` backfill worklist resupplies them), and the equivocating
/// pair must both be ordered, position decided by the hash tie-break,
/// deterministically.
pub fn run_live_topology(seed: u64, drop_pct: u8, equivocate: bool) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let producers = 2u8;
    let blocks = 2_000u64;
    let gen = gen_braid(&mut rng, producers, blocks, equivocate);
    let n = gen.views.len();
    let cfg = open_cfg(n, gen.max_height + 1);
    let drop_pct = drop_pct.min(90) as u64;
    let mut checks = Checks::default();

    let mut braids: Vec<Braid> = Vec::with_capacity(2);
    for node in 0u64..2 {
        let mut nrng = XorShift64::new(seed ^ 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(node + 1));
        // Seeded delivery schedule: drop, then bounded forward reorder (≤ 8).
        let mut delivered: Vec<usize> = Vec::new();
        let mut undelivered: Vec<usize> = Vec::new();
        for i in 0..n {
            if nrng.below(100) < drop_pct {
                undelivered.push(i);
            } else {
                delivered.push(i);
            }
        }
        for i in 0..delivered.len() {
            let w = (delivered.len() - i).min(8) as u64;
            let j = i + nrng.below(w) as usize;
            delivered.swap(i, j);
        }

        let mut b = Braid::new(cfg.clone());
        let mut bad = 0u64;
        for &i in &delivered {
            match b.insert(gen.views[i].clone()) {
                InsertOutcome::Inserted { .. }
                | InsertOutcome::MissingParents(_)
                | InsertOutcome::Duplicate => {}
                _ => bad += 1,
            }
        }
        checks.ok(bad == 0, format!("node {node}: {bad} delivered inserts rejected"));

        // Backfill: serve the braid's missing-parents worklist from the
        // generated set, then push any still-undelivered (dropped) blocks.
        let mut guard = 0usize;
        loop {
            guard += 1;
            if guard > 4 * n + 16 {
                checks.ok(false, format!("node {node}: backfill did not converge"));
                break;
            }
            let mp = b.missing_parents();
            if !mp.is_empty() {
                for h in mp {
                    match gen.by_hash.get(&h) {
                        Some(&i) => {
                            let _ = b.insert(gen.views[i].clone());
                            undelivered.retain(|&x| x != i);
                        }
                        None => checks.ok(false, format!("node {node}: foreign missing parent")),
                    }
                }
                continue;
            }
            match undelivered.pop() {
                Some(i) => {
                    let _ = b.insert(gen.views[i].clone());
                }
                None => break,
            }
        }
        checks.ok(
            b.missing_parents().is_empty(),
            format!("node {node}: missing parents after backfill"),
        );
        checks.ok(
            gen.views.iter().all(|v| b.contains(&v.hash)),
            format!("node {node}: incomplete DAG after backfill"),
        );
        braids.push(b);
    }

    let lin0 = braids[0].linearize();
    let lin1 = braids[1].linearize();
    let div = divergence_count(&lin0, &lin1);
    checks.ok(div == 0, format!("nodes diverge at {div} positions"));
    let oh = braids[0].order_hash();
    checks.ok(oh == braids[1].order_hash(), "order_hash mismatch between nodes");
    checks.ok(lin0.len() == n, "linearization does not cover the DAG");

    if let Some((e_small, e_big)) = gen.equivocation {
        for (label, lin) in [("node 0", &lin0), ("node 1", &lin1)] {
            let ps = lin.iter().position(|h| *h == e_small);
            let pb = lin.iter().position(|h| *h == e_big);
            checks.ok(
                ps.is_some() && pb.is_some(),
                format!("{label}: equivocating pair not both ordered"),
            );
            if let (Some(ps), Some(pb)) = (ps, pb) {
                checks.ok(
                    ps < pb,
                    format!("{label}: equivocation tie-break not by min-hash ({ps} !< {pb})"),
                );
            }
        }
    }

    let passed = checks.passed();
    BraidSimReport {
        scenario: "S5 live-topology",
        blocks: n as u64,
        producers,
        divergence: div,
        order_hash_hex: hex32(&oh),
        passed,
        detail: checks.detail(format!(
            "drop {drop_pct}% · reorder ≤8 · equivocation {} · 2 nodes agree after park→backfill",
            if equivocate { "both ordered, min-hash first" } else { "off" }
        )),
    }
}

// ─── S6: window / memory bounds ─────────────────────────────────────────────

/// S6 — a long honest run (default gate: 100k blocks, `final_depth` 64,
/// `max_window` 16384): the active window must never exceed the cap, the
/// finalized-prefix order must be byte-stable across every cleanup, and
/// freshly drained blocks must remain resident (the evict-below-final model
/// keeps the recent spine reachable).
pub fn run_window_bounds(seed: u64, blocks: u64) -> BraidSimReport {
    let mut rng = XorShift64::new(seed);
    let producers = 2u8;
    let gen = gen_braid(&mut rng, producers, blocks, false);
    let n = gen.views.len();
    let cfg = BraidConfig::default(); // final_depth 64, max_window 16_384
    let mut checks = Checks::default();

    let mut b = Braid::new(cfg.clone());
    let mut drains: Vec<BlockHash> = Vec::new();
    let mut bad_inserts = 0u64;
    let mut window_violations = 0u64;
    let mut max_window = 0usize;
    let mut retention_misses = 0u64;
    let mut prefix_breaks = 0u64;
    for (i, v) in gen.views.iter().enumerate() {
        if !matches!(b.insert(v.clone()), InsertOutcome::Inserted { .. }) {
            bad_inserts += 1;
        }
        let newly = b.drain_ordered();
        if !newly.is_empty() {
            for h in &newly {
                if !b.contains(h) {
                    retention_misses += 1;
                }
            }
            drains.extend(newly);
        }
        let w = b.stats().window;
        max_window = max_window.max(w);
        if w > cfg.max_window {
            window_violations += 1;
        }
        if i % 5_000 == 0 || i + 1 == n {
            let lin = b.linearize();
            if drains.len() > lin.len() || drains.as_slice() != &lin[..drains.len()] {
                prefix_breaks += 1;
            }
        }
    }
    checks.ok(bad_inserts == 0, format!("{bad_inserts} honest inserts rejected"));
    checks.ok(
        window_violations == 0,
        format!("window exceeded cap {window_violations} times (max {max_window})"),
    );
    checks.ok(
        prefix_breaks == 0,
        format!("finalized prefix broke {prefix_breaks} times across cleanups"),
    );
    checks.ok(
        retention_misses == 0,
        format!("{retention_misses} freshly drained blocks evicted immediately"),
    );

    let f = b.finalized_height();
    // Coverage sandwich: emission never passes f; and everything comfortably
    // below f (merge-parent lag ≤ 17 by the generator's height coupling) is
    // finalized.
    let upper = gen.views.iter().filter(|v| v.height <= f).count();
    let lower = gen
        .views
        .iter()
        .filter(|v| v.height <= f.saturating_sub(64))
        .count();
    checks.ok(
        drains.len() <= upper && drains.len() >= lower,
        format!("finalized coverage off ({} not in [{lower}, {upper}])", drains.len()),
    );
    if let Some(tip) = b.selected_tip() {
        checks.ok(b.is_on_spine(&tip), "selected tip not on its own spine");
    } else {
        checks.ok(false, "no selected tip after the run");
    }

    let oh = b.order_hash();
    let passed = checks.passed();
    BraidSimReport {
        scenario: "S6 window-bounds",
        blocks: n as u64,
        producers,
        divergence: window_violations + prefix_breaks,
        order_hash_hex: hex32(&oh),
        passed,
        detail: checks.detail(format!(
            "max window {max_window} ≤ cap {} · {} finalized (f={f}) · prefix stable across cleanups",
            cfg.max_window,
            drains.len()
        )),
    }
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1_dual_instance_agrees() {
        let r = run_dual_instance(1, 3, 1_500);
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.divergence, 0);
    }

    #[test]
    fn s2_permutations_invariant() {
        let r = run_permutation_invariance(2, 8);
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.divergence, 0);
    }

    #[test]
    fn s3_withheld_attacker_contained() {
        let r = run_withheld_attacker(3, 16);
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.divergence, 0);
    }

    #[test]
    fn s4_tamper_rejected() {
        let r = run_tamper_reject(4);
        assert!(r.passed, "{}", r.detail);
    }

    #[test]
    fn s5_live_topology_converges() {
        let r = run_live_topology(5, 30, true);
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.divergence, 0);
    }

    #[test]
    fn s5_clean_topology_converges() {
        let r = run_live_topology(6, 0, false);
        assert!(r.passed, "{}", r.detail);
    }

    #[test]
    fn s6_window_bounded() {
        let r = run_window_bounds(7, 20_000);
        assert!(r.passed, "{}", r.detail);
    }

    #[test]
    fn seeded_runs_are_bit_reproducible() {
        let a = run_dual_instance(42, 2, 800);
        let b = run_dual_instance(42, 2, 800);
        assert_eq!(a.order_hash_hex, b.order_hash_hex);
        assert_eq!(a.passed, b.passed);
    }
}
