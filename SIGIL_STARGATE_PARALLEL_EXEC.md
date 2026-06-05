# Stargate #2 — parallel execution (measured) + the both-levers verdict

> chronos wind tunnel, release/Epsilon (48 cores). parallel_exec.rs.
> Correctness anchor: parallel commutative-merge root == serial fold root,
> asserted at EVERY thread count — the soundness proof. — rocky-sigil 2026-05-30

## Measured (4M txs, 1M accounts, per-thread accumulator)

| threads | TPS | speedup | clears 1M? |
|---|---|---|---|
| 1 | 531,620 | 1.0× | — |
| 2 | 1,009,414 | 1.9× | ✓ |
| 4 | 1,901,939 | 3.6× | ✓ |
| 8 | 3,226,609 | 6.1× | ✓ |
| 16 | 5,173,981 | **9.7×** | ✓ (peak) |
| 32 | 4,316,709 | 8.1× | ✓ (NUMA regression) |
| 48 | 4,966,461 | 9.3× | ✓ |

## Verdict

1. **Stargate's 1M TPS criterion is MET in single-machine sim.** Parallel
   execution clears 1M at 2 threads, peaks ~5M at 16. Combined with #1
   (incremental roots, 5.3× already), the TPS target holds with margin.

2. **The leaf hash parallelizes for FREE.** The additive accumulator's
   partial sums merge commutatively — proven: parallel root == serial root
   at every thread count. So parallel execution IS the sound form of
   "batched BLAKE3" (across cores, not SIMD lanes). #1b's hash cost is
   absorbed by #2's cores. No faster hash needed to hit the target; within-
   core SIMD batch is unnecessary (cross-core clears the goal by 5×).

3. **HONEST CORRECTION to the earlier "48× headroom" claim.** Measured
   scaling plateaus ~16 threads at ~5M TPS (~10×, NOT 48×). Memory bandwidth
   + NUMA cap it — note the 32-thread regression (Epsilon's 2 NUMA nodes;
   crossing the boundary hurts). The real ceiling is ~10×, still 5× over the
   1M target. The wind tunnel corrected the guess, as designed.

## What's left for the FULL Stargate criteria

- ✓ ≥1M TPS — met (incremental roots + parallel exec, single machine)
- ☐ ≥10,000 blocks/sec into a DAG — needs DagKnight + Narwhal (the sigil-dag
  chronos harness, not this single-machine measurement)
- ☐ ≤1ms cluster finality — pipelining + responsive consensus
- ☐ zero safety regressions at scale — the 8 adversarial scenarios green
- ☐ late-join resync into a running DAG

The single-machine TPS wall is moved + measured. The DAG (blocks/sec) and
finality walls are the next chronos harness (sigil-dag), modeling N parallel
producers — simulatable before a line of production consensus, per the plan.

## NUMA follow-up (optional refinement)

The 16→32 regression is pinnable: shard threads to NUMA nodes, keep each
shard's state node-local. Would lift the plateau past 16. Worth ~2× more if
1M isn't enough margin — but it already is, so this is post-target polish.

— rocky-sigil 🟣
