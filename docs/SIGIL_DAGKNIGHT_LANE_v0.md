# SIGIL_DAGKNIGHT_LANE_v0 — "make the braid real" (DAGKNIGHT-1 design)

**Date:** 2026-07-02 (Epsilon, read-only design phase)
**Owner:** DAGKNIGHT-1 design agent
**Inputs:** Scout A (q-dag-knight reality audit), Scout B (sigil seam map) — key claims re-verified against source on 2026-07-02; every file:line below was read this session.
**Scope:** `crates/sigil-dagknight` (new) + `crates/flux-topology` (new, QTFT-1) + minimal `sigil-node` wiring behind `SIGIL_DAG=1`.
**Hard constraints honored:** zero header schema change; `SIGIL_DAG=0` ⇒ behavior-identical; `commit_state_transition` + 4-root enforcement inviolable; sim gate is in-crate deterministic (no network, no sigil-chronos edits — that crate is claimed by another agent); honest naming.

---

## 1. Algorithm decision

**Decision: HYBRID — port the one sound substrate, extend the one proven comparator, write the ordering rule fresh. Name it what it is.**

| Candidate | Verdict | Why (verified) |
|---|---|---|
| `q-dag-knight` consensus modules (lib.rs / ordering_rules / commit_logic / anchor_election / voting_coordinator) | **DO NOT PORT** | Ordering rule is degenerate — DFS topo sort immediately overwritten by lexicographic vertex-ID sort (`q-dag-knight/src/ordering_rules.rs:183-186`); anchor election receives exactly one candidate and elects it (`anchor_election.rs:206-335`); commit decisions carry `transactions = vec![]` behind TODOs (`commit_logic.rs:348-350`); zero GHOSTDAG machinery anywhere (grep blue/k_cluster/selected_parent/mergeset: 0 hits); test suite is largely mocks/non-compiling (`tests/consensus_voting_tests.rs` imports zero crate code; lib.rs tests call `.is_ok()` on un-awaited `async fn new` futures, lib.rs:105 vs 868-879). Also drags q-narwhal-core / q-lattice-vrf / q-quantum-rng. |
| `q-dag-knight/src/simd_sets.rs` `BitfieldDag` | **PORT** (~700 LOC incl. 10 real tests) | The only genuinely sound piece: transitive past/future bitfields (`simd_sets.rs:301-336`), anticone = active & !past & !future (`:86-99`), O(1) `causally_precedes` (`:437-444`), deterministic Kahn topo sort (`:361-434`). Sole external dep is `q_types::VertexId = [u8;32]` (q-types/src/lib.rs:2404) — trivially replaced by `sigil_header::BlockHash`. Dead code upstream (feature `simd-dag` enabled by no consumer) so porting it orphans nothing. **Port fix:** the Kahn frontier re-sorts the whole queue with an inconsistent reversed comparator (`simd_sets.rs:392-431`) — replace with a `BTreeSet` frontier keyed `(height, producer, hash)`. |
| `sigil-state/examples/stargate_dag.rs` `linearize()` | **EXTEND** (9 LOC comparator, proven divergence=0) | `sort_unstable_by (round, producer, hash)` (`stargate_dag.rs:65-73`), proven on a ~1M-block braid with two independent passes + hard assert (`:166-179`). **Known hole:** the sort never reads `merge_parents` — topological validity held only because the harness guaranteed parents got smaller keys (`:151-165`). The fresh core keeps the comparator as the TIE-BREAK but gates every emission on "all parents already emitted" via the ported bitfield past-sets. |
| `flux-p2p/src/dagknight.rs` committee-BFT prototype | **REFERENCE ONLY** | Honest round-BFT shape (2f+1 references, hash leader, commit rule `dagknight.rs:364-423`) but fixed validator set, empty signatures (`:281`), simulated VDF (`:434-441`), self-admitted "Simplified check" (`:386-388`). Architecture mismatch for SIGIL's open producer braid. Keep as the shape sketch for a future finality-committee layer. |
| GHOSTDAG-fresh (greedy k-cluster blue set) | **v2, explicitly scoped, NOT v1** | ~400–800 LOC over the ported anticone primitive. Do it only when hash-grinding tie-break abuse or anticone-based selfish mining becomes a live requirement. Priced; not promised. |

### The v1 ordering rule ("deterministic braid linearization")

Pure function of the DAG set (hence permutation-invariant by construction):

1. **DAG** = blocks with edges `parent_hash` (spine edge) + `merge_parents` (merge edges), both already committed AND producer-signed in the live header (`sigil-header/src/lib.rs:184-190`, hashed via `hash()` at `:245-254`, signed via `signing_bytes()` at `:260-264`). Zero wire change.
2. **Linearization** = Kahn topological sort where the ready-frontier is a `BTreeSet` ordered by `(height, producer, hash)`; emit min; a block enters the frontier only when ALL of `{parent_hash} ∪ merge_parents` have been emitted (or are pre-window/finalized). Parents-before-children is therefore guaranteed on arbitrary DAGs — the stargate hole is closed.
3. **Selected spine** = walk `parent_hash` links back from the selected tip; selected tip = max height, tie-break min hash. (Nakamoto-ish; hash tie-break is grindable — documented v1 limitation, the exact thing GHOSTDAG v2 bounds.)
4. **Finality window** = `final_depth` (default 64, env `SIGIL_DAG_FINAL_DEPTH`): once the selected tip is ≥ `final_depth` above height *h*, the linearized prefix through *h* is frozen. Insertions at height ≤ finalized height are refused from ordering (`InsertOutcome::BelowFinal`) — the reorder window is enforced by construction, the DAG analog of the sync-down guard.
5. **order_hash** = BLAKE3 chained over the linearized hashes — the one-word divergence detector two nodes compare.

### Honest-naming statement (binding)

The crate directory stays `crates/sigil-dagknight` (it is the long-planned Track-A lane name — `sigil-node/Cargo.toml` description already references "flux-dagknight (Track A) in P2"), **but the crate description, `lib.rs` module header, and all public API names say what v1 actually is: DETERMINISTIC BRAID LINEARIZATION.** Specifically:

> "sigil-dagknight — the DAG-ordering lane crate. **v1 implements deterministic braid linearization** (topologically-guarded total order over the committed `parent_hash`/`merge_parents` DAG, `(height, producer, hash)` tie-break, fixed finality window). **It is NOT GHOSTDAG** (no blue score / k-cluster / selected-parent-by-blue-work) **and NOT DagKnight-the-paper** (no parameterless min-cut anticone bound). GHOSTDAG-style greedy k-cluster ordering is the scoped v2 upgrade; DagKnight-the-paper is future research and is not promised."

Public types are named `Braid` / `BraidLinearizer` semantics, never `DagKnightOrdering`. Any release notes, science_summary text, or TUI strings sourced from this lane must reproduce this paragraph's claims and nothing stronger. (This is the lesson of the PQC "from day one" incident — see `project_quillon_pqc_block_sigs_reality`.)

---

## 2. `crates/sigil-dagknight` — API

**Cargo.toml** (flux-fold pattern — `crates/flux-fold/Cargo.toml` is the exemplar; `members = ["crates/*"]` glob at root `Cargo.toml:2` auto-registers):

```toml
[package]
name = "sigil-dagknight"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Deterministic braid linearization over the committed SIGIL block DAG (parent_hash + merge_parents). NOT GHOSTDAG, NOT DagKnight-the-paper — see docs/SIGIL_DAGKNIGHT_LANE_v0.md §1."

[dependencies]
blake3 = { workspace = true }
serde  = { workspace = true }
sigil-header = { path = "../sigil-header" }
```

Also add to root `[workspace.dependencies]` (after `Cargo.toml:66`): `sigil-dagknight = { path = "crates/sigil-dagknight" }` so sigil-node pulls it `{ workspace = true }`.

### Module layout + LOC estimate (~1,550 LOC total incl. tests)

| Module | Contents | LOC |
|---|---|---|
| `src/lib.rs` | honesty header, re-exports, `BraidConfig` | ~80 |
| `src/view.rs` | `BlockView` + `From<&SigilBlockHeaderV0>` | ~60 |
| `src/bitset.rs` | ported `BitfieldDag`/`VertexBitfield`/`VertexIndexMap` from `q-dag-knight/src/simd_sets.rs` (rename `VertexId`→`BlockHash`, BTreeSet frontier fix, keep `cleanup_before_round`→`cleanup_below_height` hard window) + its 10 tests | ~500 |
| `src/braid.rs` | `Braid`: insert / tips / selected spine / batch + incremental linearize / finality window / order_hash | ~380 |
| `src/present.rs` | braid-presentation extractor for flux-topology | ~150 |
| `src/sim.rs` | deterministic adversarial sim (Report + `run_*` fns, `sigil-chronos/src/turbosync.rs:29-64` pattern replicated, NOT imported) | ~450 |
| `examples/braid_sim.rs` | the one example binary: runs S1–S6, prints reports, exits non-zero on any gate failure | ~90 |

### Public API (exact)

```rust
// ── src/view.rs ──────────────────────────────────────────────────────────
/// The ordering layer's view of a block — extracted from the committed
/// header. The crate never sees transitions/state; bodies stay in sigil-node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockView {
    pub hash: BlockHash,               // sigil_header::BlockHash = [u8;32]
    pub parent: BlockHash,             // header.parent_hash (spine edge)
    pub merge_parents: Vec<BlockHash>, // header.merge_parents (merge edges)
    pub height: u64,
    pub producer: [u8; 32],            // header.producer (ValidatorId)
}
impl From<&SigilBlockHeaderV0> for BlockView { /* field copy */ }

// ── src/lib.rs ───────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct BraidConfig {
    pub final_depth: u64,   // default 64  (env SIGIL_DAG_FINAL_DEPTH in node)
    pub max_window: usize,  // default 16_384 active (non-finalized) views
    pub max_pending: usize, // default 4_096 parent-missing views
    pub max_merge_parents: usize, // default 4 — reject headers exceeding it
}
impl Default for BraidConfig { /* above */ }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Accepted; `newly_ready` blocks became orderable (drain them).
    Inserted { newly_ready: usize },
    /// Already present — no-op, order unchanged.
    Duplicate,
    /// Parked in the pending set; caller should backfill these hashes.
    MissingParents(Vec<BlockHash>),
    /// Height ≤ finalized height — refused from ordering (reorg window guard).
    BelowFinal { finalized: u64 },
    /// Structurally invalid (self-parent, dup merge parent, > max_merge_parents,
    /// height not parent.height+1, pending overflow). Never panics.
    Rejected(&'static str),
}

// ── src/braid.rs ─────────────────────────────────────────────────────────
pub struct Braid { /* bitset dag + frontier + pending + finalized prefix */ }

impl Braid {
    pub fn new(cfg: BraidConfig) -> Self;
    /// Seed with the genesis view (height 0). Must be first insert.
    pub fn insert(&mut self, view: BlockView) -> InsertOutcome;  // incremental, O(deps·window/64)
    /// Current DAG tips (no known children), minus `exclude` — the producer's
    /// merge_parents source. Deterministic order (height desc, hash asc), capped.
    pub fn merge_tips(&self, exclude: &BlockHash, cap: usize) -> Vec<BlockHash>;
    pub fn selected_tip(&self) -> Option<BlockHash>;             // max height, min hash
    pub fn is_on_spine(&self, h: &BlockHash) -> bool;            // parent-walk from selected_tip
    /// BATCH: full deterministic linearization of the non-finalized window,
    /// appended to the frozen prefix. Pure fn of DAG contents.
    pub fn linearize(&self) -> Vec<BlockHash>;
    /// INCREMENTAL: newly stable-ordered blocks since last drain. Invariant
    /// (sim-gated): concat(all drains) == linearize() over the same DAG.
    pub fn drain_ordered(&mut self) -> Vec<BlockHash>;
    pub fn finalized_height(&self) -> u64;
    /// BLAKE3 chain over the linearized order — the divergence detector.
    pub fn order_hash(&self) -> [u8; 32];
    pub fn contains(&self, h: &BlockHash) -> bool;
    pub fn missing_parents(&self) -> Vec<BlockHash>;             // backfill worklist
    pub fn stats(&self) -> BraidStats;                           // window/pending/finalized/tips counts
}

// ── src/present.rs ───────────────────────────────────────────────────────
/// Artin-generator braid word for the finalized window [from_h, to_h]:
/// strands = producers seen in-window (strand index = rank of producer id,
/// ascending — deterministic); crossings emitted per linearization step where
/// adjacent-strand order swaps; sign σ_i = +(i+1) if the overtaking block's
/// producer id < the overtaken strand's producer id, else -(i+1). A
/// PRESENTATION CONVENTION over the deterministic order — documented as such,
/// no physical-braid claim. Merge edges between strands contribute the
/// crossings; a linear chain (no merges) yields the empty word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BraidPresentation {
    pub strands: u32,
    pub word: Vec<i32>,           // ±(i+1) Artin generators, 1-based
    pub producers: Vec<[u8; 32]>, // strand index → producer id
}
impl Braid {
    pub fn braid_word(&self, from_height: u64, to_height: u64) -> BraidPresentation;
}

// ── src/sim.rs (the VARFLOW gate — §4) ──────────────────────────────────
pub struct BraidSimReport { /* pub fields + summary() one-liner, turbosync.rs:29-62 shape */ }
pub fn run_dual_instance(seed: u64, producers: u8, blocks: u64) -> BraidSimReport;         // S1
pub fn run_permutation_invariance(seed: u64, perms: u32) -> BraidSimReport;                // S2
pub fn run_withheld_attacker(seed: u64, fork_depth: u64) -> BraidSimReport;                // S3
pub fn run_tamper_reject(seed: u64) -> BraidSimReport;                                     // S4
pub fn run_live_topology(seed: u64, drop_pct: u8, equivocate: bool) -> BraidSimReport;     // S5a
pub fn run_window_bounds(seed: u64, blocks: u64) -> BraidSimReport;                        // S6
```

**Deliberate exclusions:** no state, no transitions, no networking, no tokio, no sigil-state/sigil-node deps (no cycles; sim S5b's state leg lives in sigil-node, §4). Bodies (full `Block`s) are a node concern — the crate orders `BlockView`s only.

**bitset.rs port notes:** memory is O(window²/8) bytes (three N-bit fields × N vertices) — 16,384-view window ≈ 100 MB worst case, 8,192 ≈ 25 MB; `max_window` is a hard cap and `cleanup_below_height` runs at every finalization advance (Scout A risk honored — never point it at the whole chain). `causally_precedes` (simd_sets.rs:437-444 pattern) backs `is_on_spine` and the parents-emitted gate.

---

## 3. sigil-node integration (minimal diff, verified anchors)

All edits live inside existing `if dag_mode` regions or add-only blocks; **`SIGIL_DAG=0` behavior is untouched** (the `Braid` is only constructed when `dag_mode` is true — `Option<Braid>`).

### 3.1 Setup — `main.rs:621-626`

Today: `dag_mode` env gate + `peer_tips: VecDeque<BlockHash>` + banner. Add, in the same block:

```rust
let mut braid: Option<sigil_dagknight::Braid> = dag_mode.then(|| {
    let mut b = sigil_dagknight::Braid::new(BraidConfig::from_env()); // SIGIL_DAG_FINAL_DEPTH etc.
    // seed with local chain: genesis + in-RAM window views (chain.get / tip_header)
    b
});
let mut dag_bodies: std::collections::HashMap<BlockHash, crate::block::Block> = HashMap::new();
```

`dag_bodies` = RAM, hash-keyed full bodies, bounded: evict everything below `braid.finalized_height()` after each drain; hard cap `SIGIL_DAG_MAX_BODIES` (default 32,768) — on overflow drop lowest-height off-spine first (the linear path's `pending` cap 200,000 at `main.rs:1053` is the precedent). Keep `peer_tips` declared but unused-in-dag-mode for one release (delete in cleanup pass), or remove it in the same diff — either is fine; the banner string gains "braid linearization v1" wording per §1.

### 3.2 Receive path — replace the stub at `main.rs:1030-1040`

Today (verified): in `dag_mode` the block's hash is pushed onto `peer_tips` (cap 4), `tx_count` counted, then `continue` — **the body is dropped, never stored/applied.** Replacement semantics:

```
1. block.header.precheck()                         // cheap; reject junk before touching the braid
2. view = BlockView::from(&block.header)
3. match braid.insert(view):
     Inserted{..}   → dag_bodies.insert(hash, block); goto 4
     Duplicate      → continue
     MissingParents(ps) → dag_bodies.insert(hash, block) (park);
                          throttle-reuse the EXISTING BackfillReq path
                          (main.rs:1056-1075 shape: rr request by height range
                          around the missing parents; same last_req throttle)
     BelowFinal{..} → metric ++ ; continue          // reorg-window guard held
     Rejected(r)    → metric ++ ; eprintln; continue
4. for h in braid.drain_ordered():                  // newly stable-ordered
     if braid.is_on_spine(&h)
        && let Some(b) = dag_bodies view where
           b.header.parent_hash == chain.parent_hash()
        && b.header.height == chain.height():
           chain.apply(b.clone())?                  // FULL existing chokepoint:
                                                    // precheck → commit_state_transition
                                                    // → check_roots_match → window prune
                                                    // (chain.rs:106-145, VERBATIM, unmodified)
           chain_log.append_bytes(&bytes)           // ordered linear output ONLY (main.rs:1086 pattern)
     else: spine_skip metric ++                     // off-spine or non-extending: ordered,
                                                    // committed, NOT state-applied (v0)
5. evict dag_bodies below finalized_height
```

**Key property: zero `chain.rs` changes in v0.** Spine blocks that extend the local tip already satisfy every `ChainTip::apply` check (height==tip+1 at `chain.rs:110`, parent==tip at `:116`); the braid only decides *which* block is next. The 4-root enforcement (`chain.rs:133-137` via `block.check_roots_match`, `block.rs:37-42`) and the `commit_state_transition` chokepoint (`chain.rs:130`) are untouched — the inviolables stay inviolable. A `ChainTip::apply_ordered(...)` with parent/height override (Scout B's suggestion) is **explicitly deferred to v1** (spine-follow-with-reorg), because it is the highest-blast-radius edit in the plan and v0 doesn't need it.

Per-event cost is O(small): one insert (bitfield union over ≤5 parents) + drain; the select loop (`main.rs`) stays unstarved (the serve-throttle starvation class at `main.rs:660-676` is the cautionary precedent).

### 3.3 Producer path — `main.rs:731-737`

Today: `mp = peer_tips snapshot if dag_mode` → `mint_next_block(&chain, mp, &block_txs)` (`main.rs:737`), which sets `header.merge_parents` (`main.rs:1832`). One-line change: source tips from the braid instead of the 4-deep VecDeque:

```rust
let mp: Vec<BlockHash> = if let Some(b) = braid.as_ref() {
    b.merge_tips(&chain.parent_hash(), 4)   // real DAG tips, own-tip excluded, deterministic
} else { Vec::new() };
```

After the producer self-applies (`main.rs:757`) it must also `braid.insert(BlockView::from(&header))` + `dag_bodies.insert(...)` so its own blocks are in the DAG (2 lines inside the existing `Ok(_)` arm). `mint_next_block` itself (`main.rs:1802-1859`) is unchanged.

### 3.4 What v0 explicitly does NOT do (honest semantics)

- **NO full DAG state-merge.** Ordering + commitment are real; **state transitions remain spine-only**, and only for spine blocks that *extend* the local tip. Off-spine (merge) blocks are stored, deterministically ordered, and their `txs_merkle_root`/`tx_count` commitments stand — but **their txs are never applied to state in v0** (dedup/conflict semantics = v1).
- **NO reorg / spine switch.** If the braid's selected spine departs from the local `ChainTip` (which is by construction the steady state between two v0 producers, since each self-applies its own chain and today's transitions are empty — `mint_next_block` builds `mutations: vec![]` at `main.rs:1816`, so all spines carry identical roots), the node logs `spine_mismatch` metrics and keeps its own chain. It does NOT rewind (`commit_state_transition` has no undo) and does NOT exit-78 — the divergence latch (`main.rs:1096-1108`) stays reserved for true root divergence. A pure follower (non-producing, `SIGIL_DAG=1`) DOES track the selected spine cleanly — this is the new capability v0 actually ships (today's dag_mode follower applies nothing at all).
- **NO GHOSTDAG blue-score ordering, NO DagKnight min-cut** (§1). Hash tie-break grindability is accepted + documented for v1.
- **NO chain_log format change.** `chain_log` receives only the locally-applied linear sequence, exactly as today (`chain_log.rs` height→offset index assumption preserved; DAG bodies never enter it). No flux-db writes in v0 (hash-keyed `b"dag/"` CF on the `store.rs:135-185` `BlockStore` pattern is the priced v0.5 stretch, Lane D2).
- **NO header/wire change of any kind** — `merge_parents` is already committed + signed (`sigil-header/src/lib.rs:184-190,245-264`); gossip stays serde_json on `TOPIC_BLOCKS`; backfill stays the existing rr channel. (Reminder why: `hash()` re-serializes the whole struct via serde_json, so ANY added field — even `#[serde(default)]` — retroactively breaks historical hashes.)

---

## 4. The sim gate (VARFLOW) — in-crate, deterministic, no network

Lives in `sigil-dagknight/src/sim.rs` (+ inline `#[test]`s calling each `run_*`) + `examples/braid_sim.rs`. Pattern = `sigil-chronos/src/turbosync.rs` Report-struct + pure-fn shape (`turbosync.rs:29-64`) **replicated, not imported** (sigil-chronos is another agent's lane). All scenarios seeded (`u64` seed → own tiny xorshift; no `rand` dep needed), so every run is bit-reproducible.

DAG generator: `P` producers, each minting on its own spine, cross-referencing other producers' tips as `merge_parents` per a seeded schedule, with a seeded delivery schedule (delay/drop/reorder mask) — a faithful miniature of the live 2-producer dag_mode topology (empty transitions, diverging heights, tips-exchange entanglement).

| # | Scenario | Construction | Pass criteria |
|---|---|---|---|
| S1 | **Dual-instance agreement** | Same generated DAG (P=2..4, N≥10k) fed to two independent `Braid` instances in two different arrival orders | `linearize()` vectors identical; `order_hash` equal → **divergence = 0** |
| S2 | **Permutation invariance** | K=16 seeded arrival permutations of one DAG; also: per-permutation, concat of incremental `drain_ordered()` vs one batch `linearize()` | all 16 order_hashes equal; **incremental == batch** for every permutation |
| S3 | **Withheld attacker chain** | Honest DAG grows past finality; attacker releases (a) a private fork branching *below* `finalized_height`, (b) a fork *inside* the window | (a) every insert → `BelowFinal`, finalized-prefix order_hash byte-identical before/after; (b) only the non-final suffix may reorder — prefix through `finalized_height` unchanged. **Reorder never exceeds the designed window** |
| S4 | **Tamper-reject** | Unknown/corrupted parent hash → parked (`MissingParents`) forever, never ordered; self-parent / duplicate merge-parent / height ≠ parent+1 / >4 merge parents → `Rejected`; duplicate insert → `Duplicate` with order_hash unchanged | every malformed input yields a **structured rejection — no panic, no silent accept, no order perturbation** |
| S5a | **Live-topology replica (ordering leg)** | 2 producers, empty transitions, seeded gossip drop (0–30%) + delayed delivery + an equivocating producer (two blocks, same height, same producer) | order agreement holds (S1 criteria) with drops (parked → backfilled → converge); equivocations both ordered, position decided by hash tie-break, deterministically |
| S5b | **Spine-apply leg (in sigil-node)** | Inline `#[cfg(test)]` in sigil-node (chain.rs test-mod style): follower drives a braided 2-producer block set through `Braid` + the REAL `ChainTip::apply` | follower applies exactly the selected spine through the full chokepoint (roots checked every block); off-spine blocks skipped; 0 divergence |
| S6 | **Window/memory bounds** | 100k blocks, `final_depth=64`, `max_window=16384` | active window ≤ cap at all times; finalized-prefix order_hash constant across every cleanup; `dag_bodies`-eviction model asserted (evict-below-final leaves spine reachable) |

**Gate rule:** `examples/braid_sim.rs` runs S1–S6 and exits non-zero on any failure; Lane D (node wiring) may not merge until the gate binary passes on the isolated target dir, and the four headline numbers (divergence=0, perms=16/16, prefix-immutable under attack, tamper-rejects structured) go in the commit message.

---

## 5. `crates/flux-topology` (QTFT-1) — API sketch

**Zero sigil deps** (pure math over a plain braid word) so it later lifts into the flux workspace unchanged. sigil-dagknight does NOT depend on flux-topology and vice versa — glue is one struct-literal line at the call site (`BraidWord { strands: p.strands, gens: p.word.clone() }`). Cargo.toml = flux-fold pattern; deps: none beyond `serde` (optional).

```rust
// src/lib.rs
/// A braid word in Artin generators: gens[k] = ±(i+1) means σ_i^{±1}
/// (1-based generator index, sign = crossing sign). strands = n.
pub struct BraidWord { pub strands: u32, pub gens: Vec<i32> }
pub fn permutation(w: &BraidWord) -> Vec<usize>;   // induced strand permutation
pub fn writhe(w: &BraidWord) -> i64;               // Σ signs

// src/linking.rs — REAL, O(len(word)) single sweep tracking strand positions
pub fn linking_matrix(w: &BraidWord) -> Vec<Vec<i64>>;  // lk(i,j) = ½·signed crossings between strands i,j (closure)
pub fn linking_number(w: &BraidWord, i: usize, j: usize) -> i64;

// src/laurent.rs — exact integer Laurent polynomials ℤ[t, t⁻¹]
pub struct LaurentPoly { /* min_exp: i32, coeffs: Vec<i128> */ }
impl LaurentPoly { pub fn one(); pub fn t(); add/sub/mul/neg;
    pub fn normalize_alexander(&self) -> LaurentPoly;  // canonical ±t^k unit: symmetric, Δ(1)=±1 check
    pub fn div_exact(&self, rhs: &LaurentPoly) -> Option<LaurentPoly>; }

// src/burau.rs — reduced Burau representation, (n-1)×(n-1) over LaurentPoly
pub fn burau_reduced(w: &BraidWord) -> Vec<Vec<LaurentPoly>>;  // word product, σ_i and σ_i⁻¹ generators

// src/alexander.rs — REAL, polynomial-time (matrix mult O(len·n³) + det)
/// Δ(t) of the CLOSURE of w: det(B̄(w) − I) · (1−t)/(1−tⁿ), normalized.
/// n ≤ producers-in-window (≤8 in v0) ⇒ det via fraction-free Bareiss is trivial.
pub fn alexander_poly(w: &BraidWord) -> LaurentPoly;
```

**KATs (inline tests, non-negotiable):**

| Input (braid closure) | Invariant | Expected |
|---|---|---|
| σ₁ on 2 strands (unknot) | Δ(t) | 1 |
| σ₁³ on 2 strands (**trefoil**) | Δ(t) | ≐ t − 1 + t⁻¹ (up to ±t^k) |
| (σ₁σ₂⁻¹)² on 3 strands (**figure-eight**) | Δ(t) | ≐ t − 3 + t⁻¹ |
| σ₁² on 2 strands (**Hopf link**) | linking_number(0,1); writhe | ±1; 2 |
| σ₁σ₂ on 3 strands (unknot closure) | Δ(t); permutation | 1; 3-cycle |
| far-commutation: swap σ_i σ_j (|i−j|≥2) in random words | linking_matrix, Δ | invariant (property test, seeded) |

LOC estimate: ~850 (laurent 220, burau 180, alexander 160, linking 90, lib 80, tests 320 — overlap counted once).

**Interface from sigil-dagknight (designed now, consumed by QTFT-2 later):** `Braid::braid_word(from_h, to_h) -> BraidPresentation` (§2) is the per-window strand-crossing extraction; crossings derive from the deterministic linearization (a presentation convention, stated as such — no physical claim). Cost: O(crossings) = O(merge edges in window).

**QTFT-2 header commitment — decided: ride the EVENT LOG, defer the wiring.** The header has no free aux field, and any new field breaks historical hashes (§3.4). The `StarkProof.bytes` vec is empty in Phase 0 but stuffing a topology commitment there would be dishonest labeling — rejected. The honest zero-schema-change route: QTFT-2 emits a typed `TopologyCommit` event (`window=[from_h,to_h]`, `commitment = BLAKE3(order_hash ‖ BLAKE3(braid word bytes) ‖ BLAKE3(linking matrix bytes))`) into the block's event list → hashed into the **existing `event_log_root`** → committed AND producer-signed with zero header change, and only on `SIGIL_DAG=1` blocks. That is QTFT-2 scope; v0 ships the extractor + invariants only and this paragraph is the contract.

---

## 6. Work plan — lanes, verify, rollback

One build owner; every lane lands as a compiling + tested slice. Commit hygiene: **scope-add only** `crates/sigil-dagknight/`, `crates/flux-topology/`, `crates/sigil-node/{Cargo.toml,src/main.rs}`, root `Cargo.toml` (one `[workspace.dependencies]` hunk), and the matching `Cargo.lock` hunks — **never** the pre-existing dirty `sigil-rpc` / `sigil-top` / `sigil-university` files (another agent's uncommitted work, per `git status` 2026-07-02).

| Lane | Files | Depends | LOC | Deliverable |
|---|---|---|---|---|
| **A — substrate** | `crates/sigil-dagknight/{Cargo.toml, src/lib.rs, src/view.rs, src/bitset.rs}` + root `Cargo.toml` dep hunk | — | ~640 | BitfieldDag port (BTreeSet frontier fix, height-window cleanup) + BlockView + config; the 10 ported tests + 4 new frontier-determinism tests green |
| **B — ordering** | `crates/sigil-dagknight/src/{braid.rs, present.rs}` | A | ~530 | Braid insert/linearize/drain/finality/order_hash + braid_word; unit tests for spine selection, BelowFinal, incremental==batch on small DAGs |
| **C — sim gate** | `crates/sigil-dagknight/src/sim.rs`, `examples/braid_sim.rs` | A,B | ~540 | S1–S6 all green; gate binary exits 0; headline numbers recorded |
| **D — node wiring** | `crates/sigil-node/src/main.rs` (3 regions: ~:621, ~:731, ~:1030), `crates/sigil-node/Cargo.toml` (+dep), S5b inline test | B (merge after C green) | ~170 diff | SIGIL_DAG=1 real path; SIGIL_DAG=0 behavior-identical (all edits inside `if dag_mode` / `Option<Braid>`); `fluxc check -p sigil-node` green |
| **E — flux-topology** | `crates/flux-topology/{Cargo.toml, src/lib.rs, src/laurent.rs, src/burau.rs, src/alexander.rs, src/linking.rs}` | — (fully parallel) | ~850 | All KATs green (trefoil/figure-8/Hopf/unknot + far-commutation property) |
| **D2 — stretch (optional)** | sigil-node flux-db `b"dag"` CF store (clone `store.rs:135-185` pattern, hash keys) | D | ~120 | DAG-body persistence across restarts; NOT required for the v0 gate |
| **F — verify + land** | — | all | — | full-tree `fluxc check`, run both gate binaries, single scoped commit per lane, doc cross-ref |

Parallelism: **A ∥ E** immediately; B after A; C after B; D last (and only after C's gate passes). Two agents: agent-1 = A→B→C→D, agent-2 = E (+D2). Claim files via `flux_file_claim`/swarm bus as usual.

### Build/verify commands (Epsilon dispensation — capped scope, isolated target, one build owner)

```bash
# compile-verify (per lane) — NEVER raw cargo, NEVER uncapped:
systemd-run --scope -p MemoryMax=10G -p CPUQuota=700% bash -c \
  "cd /home/storage/deepseek-codewhale/sigil && \
   CARGO_TARGET_DIR=/home/storage/dagknight-target ionice -c3 nice -n19 \
   /home/storage/deepseek-codewhale/flux/target/debug/fluxc check -p sigil-dagknight"
# same for -p flux-topology and -p sigil-node

# tests: fluxc test is broken this season (valueless --package) → fluxc check to build,
# then run the test binary directly:
ls /home/storage/dagknight-target/debug/deps/sigil_dagknight-* # newest hash
systemd-run --scope -p MemoryMax=8G bash -c \
  "/home/storage/dagknight-target/debug/deps/sigil_dagknight-<hash> --test-threads=1"

# the gate binary (example) — if fluxc lacks --example passthrough, C ships it as a
# [[bin]] "braid-sim" instead (open question OQ-1):
systemd-run --scope -p MemoryMax=8G bash -c \
  "cd /home/storage/deepseek-codewhale/sigil && CARGO_TARGET_DIR=/home/storage/dagknight-target \
   /home/storage/deepseek-codewhale/flux/target/debug/fluxc build -p sigil-dagknight --example braid_sim && \
   /home/storage/dagknight-target/debug/examples/braid_sim"
```

Never build in the live node's RAM budget; never touch the shared `.target-shared` for this lane (isolated dir above); Delta (the sanctioned SIGIL build box) is offline until ~July — this is the operator-blessed Epsilon fallback.

### Rollback path

1. **Runtime:** `SIGIL_DAG` unset/0 ⇒ the entire lane is inert (Braid never constructed; producer `mp` empty; receive path identical to today). No flag day, no migration.
2. **Persistence:** v0 writes nothing new to disk (no chain_log change, no new DB CFs) ⇒ rolling back the binary has zero data implications. (If D2 stretch lands, the `b"dag"` CF is additive and ignorable by old binaries.)
3. **Code:** each lane is one scoped commit; `git revert` in reverse lane order (D → C → B → A / E independent). No other agent's files are in any commit, so reverts can't collide with the dirty sigil-rpc/sigil-top/sigil-university work.
4. **Header/wire:** nothing to roll back — schema untouched by design.

### Open questions (for the operator / lane lead)

- **OQ-1:** does `fluxc build -p X --example Y` pass through? If not, `braid_sim` ships as `[[bin]] braid-sim` (identical content).
- **OQ-2:** sign-off on the v1 spine rule (max height, min-hash tie-break) + `final_depth=64` default — both grindable/economic parameters, both env-tunable, GHOSTDAG v2 is the priced fix (~400–800 LOC).
- **OQ-3:** QTFT-2's `TopologyCommit` event changes `event_log_root` on SIGIL_DAG=1 blocks (producer-side only) — confirm that's acceptable when QTFT-2 is chartered (it's outside this lane).
- **OQ-4:** is RAM-only `dag_bodies` acceptable for v0 (restart loses off-spine bodies; spine is in chain_log as today), or should D2 (flux-db CF) be in-scope from the start?
- **OQ-5:** crate name — `sigil-dagknight` retained as the Track-A lane name with the §1 honesty header baked into description + lib.rs; if the operator prefers strict naming, `sigil-braid` is the drop-in alternative (nothing else changes).
