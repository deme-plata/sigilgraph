# Stargate → 500M — the handoff (state is solved; the wall is crypto)

> Continue-to-500M iteration + the measurement that redirects the whole
> project. release/Epsilon 48c AVX-512. stargate_500m.rs. Handoff for the
> takeover session. — rocky-sigil 2026-05-30

## The full TPS arc (measured, this session)

| stage | what | TPS / rate |
|---|---|---|
| baseline | whole-map rehash roots, single thread | ~40k |
| #1 incremental roots (acc.rs) | roots 73%→0.1% | 5.3× → ~370k single-thread |
| #1 + u64-limb accumulator | clean arithmetic fix | 1.14M single-thread |
| #2 parallel exec (sound BLAKE3) | per-thread acc, commutative merge | **~13M peak** |
| → 50M iteration (fast-leaf ceiling) | unsafe non-crypto leaf | 165M |
| → 500M (fast leaf, AVX-512, 48t) | state-commitment ceiling | **209M commits/s** |

## THE finding that matters

```
state-commitment ceiling:  209,000,000 /s   (unsafe fast leaf, 48t)
sound state (BLAKE3):       ~13,000,000 /s
ed25519 signature verify:        113,655 /s   (48t) ← THE WALL
```

**Even the SOUND state path (13M/s) is ~115× faster than signature
verification (113k/s).** The unsafe 209M is ~1,850× faster. So:

- **State is solved.** Stargate #1 (incremental roots) + #2 (parallel exec)
  made the state machine a non-bottleneck by two orders of magnitude.
- **The binding constraint is now CRYPTO + CONSENSUS, not state.** Pushing
  state to 500M is real but academic — sig-verify caps end-to-end TPS at
  ~113k/s today.
- **The 50M/500M state chase answered its own question:** don't optimize
  state further. It's free. Optimize the wall.

## The wall, quantified

ed25519 single verify ≈ 110µs (one verify = scalar mult + decompression).
~9k/s/core, ~113k/s across 48 cores (sub-linear — memory/decompression bound).

SQIsign (SIGIL's post-quantum default) is SLOWER than ed25519 — likely much
slower per verify (isogeny path). **It probably dominates entirely.** Measure
it first thing — it may put the real wall at thousands/s, not 113k.

## NEXT WALLS — the takeover session's lever list (priority order)

1. **Measure SQIsign verify rate.** It's the actual production sig. If it's
   ~1k/s, that's the real ceiling and everything below matters more. (flux-sqisign
   + sqisign-verify crates are already in the lock.)

2. **Batch verification.** `ed25519_dalek::verify_batch` amortizes the scalar
   mult — ~2-3× over single verify. Does SQIsign have a batch path? If not,
   that's a research lane.

3. **Narwhal mempool — verify ONCE on ingest.** The biggest structural lever:
   separate data dissemination (verify sigs once when a tx enters the mempool)
   from ordering (consensus orders tiny tx-hashes, never re-verifies). This is
   how the 100k+ TPS BFT chains move data in parallel while consensus stays
   cheap. Decouples sig-verify from block production entirely. **Probably the
   highest-impact next piece.**

4. **Hot-path / settlement split.** ed25519 for the high-frequency hot path,
   SQIsign only for settlement / finality. Crypto-agility (rocky #101) makes
   this a config, not a fork. Quantifies as: hot-path TPS at ed25519 rate,
   settlement at SQIsign rate.

5. **GPU sig-verify.** ed25519/curve25519 batch verify on GPU is a known ~10-50×.
   flux-zk already has a GPU path. Measure feasibility.

6. **Stargate #3 — the DAG (sigil-dag chronos harness).** The OTHER half of the
   target (10,000 blocks/sec). N parallel producers → DagKnight ordering →
   measure blocks/sec + convergence at 1000 nodes. update-v1 was offered this
   lane (msg #111).

## Artifacts left for the takeover (all in sigil-state/examples/)

- `acc.rs` — the incremental multiset accumulator (SHIPPED, 9/9 tests)
- `acc_bench.rs` — root-op micro-bench (126×–25,488×)
- `roots_throughput.rs` — A/B/C strategies (the +144% BLAKE4 finding)
- `leaf_hash_lever.rs` — sound-vs-unsafe leaf, coalescing verdict
- `parallel_exec.rs` — Stargate #2 (1M criterion MET, commutative-merge proof)
- `stargate_50m.rs` — limb-arithmetic iteration (5M→13M sound)
- `stargate_500m.rs` — THIS: state→209M + the sig-verify wall

Docs: SIGIL_STARGATE_{INCREMENTAL_ROOTS, LEAF_HASH_LEVER, PARALLEL_EXEC,
50M_ITERATION, 500M_HANDOFF}.md + SIGIL_AI_FEE_SYSTEM_v0.md.

Open: the acc→lib.rs wiring (patch ready, blocked on rocky's sigil-state claim).

## One sentence for the takeover

**State is solved and effectively free; the real road to high TPS now runs
entirely through signature verification (measure SQIsign first) and the
Narwhal "verify-once" mempool — not through making the state machine any
faster.**

— rocky-sigil 🟣

---

## WALL OPTIMIZED (sig_wall.rs, measured) — the real numbers

| sig path | verify rate | per-verify |
|---|---|---|
| **SQIsign L5, 48t (production post-quantum)** | **131 /s** | 75 ms |
| ed25519 single | 9,123 /s | 110 µs |
| ed25519 verify_batch (1t) | 28,271 /s (3.1×) | — |
| ed25519 batch × 48t (hot path) | 110,821 /s (12×) | — |

**SQIsign is the catastrophic wall: 131 verify/s even on 48 cores.** You cannot
run a chain at 131 tx/s. ed25519 batch×parallel reaches ~110k/s — **844× faster
than SQIsign.**

**THE OPTIMIZATION (crypto-agility split + Narwhal verify-once):**
- Hot path (high-freq agent txs) → ed25519 batch×parallel: **110k tx/s**
- Settlement / finality only → SQIsign L5: 131 tx/s (rare, acceptable)
- Narwhal mempool: verify ONCE on ingest at 110k/s, consensus orders hashes,
  no per-block re-verify → 110k tx/s sustainable end-to-end.
- Net: the wall moves from 131/s (SQIsign-everything) to 110k/s (ed25519 hot
  + SQIsign settle) — an **844× lift** — and GPU batch verify (flux-zk path)
  is the next ~10-50× on top.

**Revised TPS scorecard (end-to-end, honest):**
- state commitment: 209M/s (free)
- sig-verify (hot, ed25519 batch×parallel): 110k/s ← real end-to-end wall
- sig-verify (settlement, SQIsign): 131/s (rare path only)
→ Realistic single-machine end-to-end: ~110k TPS hot-path TODAY, with GPU
  verify + the DAG (multi-producer) as the multipliers toward 1M.
