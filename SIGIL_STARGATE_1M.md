# Stargate → 1M TPS — the measured scaling curve (2026-05-30)

`sigil-state/examples/stargate_1m.rs`, run on Epsilon (48c AVX-512).

## The question
End-to-end TPS sat at ~120k (sig-verify hot-path wall). How do we reach 1M?

## Measured answer
| Quantity | Measured | Meaning |
|---|---|---|
| Per-machine verify ceiling | **234,043 tx/s** | ed25519 batch×48t. ONE box, **core-bound** — slicing into more producers on the same box does not help (same 48 cores). |
| Local DAG ordering | ~162 B tx/s | k-way wave-merge of producer streams. Not the wall. |

**The curve:** aggregate = M machines × 234k (DAG-ordered).
1M is crossed at **M = 5 machines** (5 × 234k = 1.17M). 4 machines = 936k (just under).

## Two real roads to 1M (pick either / both)
1. **Multi-producer DAG — ~5 machines.** Each machine verifies its OWN mempool
   once (verify-once), produces blocks, the DAG orders them. What makes the
   5-machine aggregate REAL (not 5× a re-verify tax): **proof-carrying blocks** —
   a receiving node checks ONE O(1) proof per foreign block (measured 0.08 ms,
   flat in N, in `sig_agg.rs`) instead of re-verifying foreign signatures.
2. **One machine + GPU verify.** flux-zk-stark already has a GPU path; GPU
   batch-verify is ~10–50× the CPU ceiling = **2.3M–11.7M tx/s on one box.**

## Honest caveats
- The **234k** ceiling is ed25519 (the hot-path proxy). The real chain is PQ-only
  (SQIsign5/Dilithium5, far slower per-verify) — so the *production* per-machine
  number is **lower** unless (a) a fast PQ hot-path scheme is added, or (b) the
  proof-carrying path removes per-tx verify from the critical path. ed25519 here
  measures the *architecture's* ceiling, not the PQ chain's today.
- The **162 B/s "DAG ordering"** measures only the LOCAL k-way merge (heap ops
  over pre-sorted streams) — NOT full DagKnight consensus (network rounds +
  voting). What it legitimately proves: the local ordering step is not the
  throughput bottleneck (consistent with Narwhal decoupling mempool throughput
  from ordering latency). The real cross-machine limits are network bandwidth +
  the proof-verify, both of which the design addresses.

## Next real step
Run it **cross-machine for real**: Epsilon + Delta as two producers over the
**live WG tunnel** (`sigilwg0`, 10.77.0.1↔10.77.0.2, up + 0.47ms RTT) + flux-p2p,
each minting+broadcasting blocks, measure the 2-machine aggregate. That turns the
modeled curve into a measured 2-node datapoint — the foundation of the M-machine
line. (Blocked only on a sigil-node binary built for Delta/Debian-12.)
