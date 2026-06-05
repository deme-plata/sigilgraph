# Project Stargate-v1 — SIGIL's high-throughput engine

> *"10,000 blocks/sec · 1,000,000 TPS · 1ms finality."*
>
> The performance target for SIGIL, inspired by Kaspa's parallel-block BlockDAG and the Narwhal/DAG-mempool line (Sui, Aleo). Named Stargate-v1 by Viktor, 2026-05-30. Drafted by rocky.
>
> **Crucially: every number below is anchored to a REAL chronos measurement of the current SIGIL state machine — not a marketing figure.** The gap between "where we are" and "Stargate" is quantified, and the bottleneck is identified empirically.

---

## The measured baseline (today, via `sigil-throughput`)

`sigil-chronos`'s throughput harness drives the **real** `apply_tx` + `commit_state_transition` pipeline at scale on Epsilon (48-core, single thread, release build):

| Workload | TPS | blocks/sec | time in execution | time in roots |
|---|---|---|---|---|
| 1,000 blocks × 100 tx (100k tx) | **40,323** | 403 | 26% | **73%** |
| 1,000 blocks × 1,000 tx (1M tx) | **82,932** | 83 | 64% | 21% |
| 10,000 blocks × 100 tx (1M tx) | **41,508** | 415 | 26% | **73%** |

Two findings that set the whole roadmap:

1. **Single-threaded, the chain already does ~40–83k TPS.** Not 1M, but a real, respectable floor — and it's one core.
2. **The bottleneck is root computation, not transaction execution.** When blocks are many (small), **73% of wall-clock is spent re-hashing the four state roots** — because Phase-0 rehashes the *entire* state map every block (O(n)). Make the blocks bigger (1,000 tx) and roots amortize down to 21%, execution dominates, and TPS nearly doubles to 83k. This is the empirical case for two roadmap pieces at once: incremental roots + batching.

**So the path to Stargate isn't a mystery — chronos already shows exactly where the time goes.**

---

## The four Stargate-v1 targets, and how each is reached

### 1. 1,000,000 TPS

**Math:** 1,000 tx/block × 1,000 blocks/sec = 1M. Or 100 tx/block × 10,000 blocks/sec. Either factoring works.

**Gap from baseline:** 83k → 1M is ~12×. Two multipliers close it:
- **Incremental Sparse Merkle Tree roots** — replace the O(n) whole-map rehash with O(log n) touched-leaf updates. The harness shows roots are 73% of the time on small blocks; an SMT makes that near-free, recovering most of the wall-clock. *Highest-priority single change — measured.*
- **Parallel execution** — apply non-conflicting txs across all 48 cores (Block-STM / Sui-style optimistic parallelism). SIGIL's four-root partition (wallet/dex/event/contract) + per-account independence means most txs don't conflict. 48 cores × the per-core rate, minus contention, comfortably clears 1M.

### 2. 10,000 blocks/sec

A single producer minting sequentially is capped by "wait for the last block." **The unlock is a BlockDAG** (the Kaspa picture): many producers minting *in parallel*, every block kept, ordered after the fact.

- **DagKnight consensus** (already named in SIGIL's whitepaper) — the parameterless, *responsive* successor to GhostDAG. Producers reference multiple parent-tips; DagKnight computes a total order. Block rate decouples from propagation delay.
- **Narwhal-style mempool** (also named) — separate *data dissemination* (tx batches gossiped continuously) from *ordering* (consensus orders tiny hashes, not payloads). This is how the 100k+ TPS BFT chains move data in parallel while consensus stays cheap.

chronos already models parallel producers + per-edge latency/loss, so a 10,000-block/sec, 1,000-node DAG is **simulatable before a line of production consensus is written.**

### 3. 1ms finality

**Honest physics first:** finality cannot beat the speed of light. 1ms ≈ 200 km of fiber round-trip. So **1ms finality is a low-latency-cluster target** (co-located / same-datacenter validators), where RTT is sub-millisecond. Cross-continent finality is bounded at ~30–100ms by geography — no protocol escapes this.

Within that honest frame, 1ms is reachable via:
- **DagKnight's responsiveness** — it finalizes as fast as the network actually is, not at a fixed conservative timeout. In a tight cluster, that's ~RTT.
- **Pipelining** — propagate / order / execute / commit overlap as stages, so finality latency ≈ one stage, not the sum.
- **Optimistic local finality** — a tx is *locally* final the instant its block's roots verify (sub-ms), with global ordering catching up asynchronously.

**Stated precisely: ≤1ms finality in a low-latency validator cluster; geographically-bounded (~tens of ms) globally.** Anyone promising 1ms *global* finality is selling something light can't deliver.

### 4. The BlockDAG itself (the Kaspa picture, made ours)

Stargate's visual + structural core: parallel blocks flowing into a DAG, DagKnight-ordered, each carrying SIGIL's four state-roots + tip-proof. The `multiverse.html` viz already renders branch divergence; a Stargate viz renders the live DAG braid (the Kaspa Graph Inspector aesthetic, obsidian+violet).

---

## The engineering roadmap (priority by measured impact)

| # | Piece | Why (measured) | Unlocks |
|---|---|---|---|
| **1** | **Incremental SMT roots** | roots = 73% of wall-clock on small blocks | most of the path to 1M TPS, single-handedly |
| **2** | **Parallel execution (Block-STM)** | execution is the residual cost after #1 | 48× headroom → clears 1M |
| **3** | **DagKnight DAG consensus** | single-chain caps block-rate | 10,000 blocks/sec |
| **4** | **Narwhal mempool** | consensus bandwidth ≠ data bandwidth | sustains the DAG's data rate |
| **5** | **Batching (bigger blocks)** | 100→1,000 tx/block nearly 2× TPS | already proven in the harness |
| **6** | **Pipelining** | finality = sum of stages today | ≤1ms cluster finality |

#1 and #5 are the cheapest + highest-impact, and the harness *already proves* their value. They're the obvious first sprint.

---

## chronos is the wind tunnel

The reason Stargate is a plan and not a wish: **every piece can be measured in simulation before it's built.**

- `sigil-throughput` already measures execution TPS + the roots bottleneck (done).
- A `sigil-dag` harness (next) models N parallel producers minting into a DAG, runs DagKnight ordering, measures blocks/sec + ordering convergence at 1,000 nodes — in virtual time.
- The scenario library + property fuzzer guarantee that none of these throughput changes break safety (the eight adversarial scenarios + upgrade-compat already gate it).
- "Resync / play it all" — the late-join catch-up (sync.rs) lets a node join a running 1M-TPS DAG and prove it reaches the tip.

**Build in sim → find the wall → move it → write production code → re-measure.** That loop is why the chronos suite was built first.

---

## Stargate-v1 acceptance criteria

Stargate-v1 ships when chronos demonstrates, in a 1,000-node simulation:

- [ ] ≥ 1,000,000 TPS sustained (incremental-SMT + parallel-exec)
- [ ] ≥ 10,000 blocks/sec into the DAG (DagKnight + Narwhal)
- [ ] ≤ 1ms finality in a low-latency cluster topology (pipelined + responsive)
- [ ] Zero safety regressions — the 8 adversarial scenarios + upgrade-compat all green at scale
- [ ] A late-joining node resyncs into the running DAG and reaches the tip

Then, and only then, it goes to real Delta + Epsilon hardware — with the chronos run as the pre-flight that justifies it.

---

*Baseline measured 2026-05-30 via `sigil-chronos::throughput` on Epsilon. Targets are the destination; the gap is quantified, not guessed.* — rocky 🟠
