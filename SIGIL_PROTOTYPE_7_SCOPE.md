# SIGIL Prototype 7 — "Scale the Wall"

**Author:** rocky-sigil
**Date:** 2026-05-30
**Codename:** P7 / "the wall relocates"
**Status:** scope locked by Viktor 2026-05-30. 6 sub-tasks open. crypto@flux joined the swarm for this prototype.

---

## One-line goal

> *Make crypto verification match state throughput so the wall stops being the wall.*

Target: **1M+ signatures verified per second sustained over a 1h soak**, on the same hardware where state hit **209M ops/sec** in the Stargate 500M handoff.

## Why P7 exists

The Stargate 500M measurement found:

| Layer | Throughput | Status |
|---|---|---|
| State (apply, propagate, commit, roots) | **209,000,000 ops/sec** | solved |
| Crypto (signature verification @ 48 threads) | **113,655 ops/sec** | the wall |
| Ratio | **~1840× imbalance** | crypto is the bottleneck |

The 500M handoff doc states the directive: **"Stop optimizing state. Start scaling crypto."** P7 is the response.

Until crypto scales, every other improvement (settlements-per-second, agent economy, soak resilience, USDS issuance) is bounded by the 113K signature verification cap. P7 unblocks every downstream prototype.

## Sub-tasks (claim by reply on the swarm broadcast)

### P7-A — Batch SQIsign verification

Verify N SQIsign signatures in approximately the time it takes to verify 1. Pedersen-style batching: aggregate N (msg, sig, pubkey) triples into one randomized linear combination, verify the aggregate. Probabilistic with statistical-soundness bound; tunable to crypto-level confidence.

**Composes with:** flux-sqisign, flux-zk-stark.
**LOC:** ~400.
**Settlement:** 1.5 QUG.
**Measurement gate:** verify 1024 signatures in ≤2× the time it takes to verify 1.

### P7-B — BLS aggregation for validator quorums

Replace per-validator individual signatures with one aggregated BLS signature. M-of-N validator quorum becomes one aggregate sig + one verify. Composes naturally with flux-quorum (#57 in update-v1).

**Composes with:** flux-sqisign (alternative), flux-quorum, flux-witness.
**LOC:** ~600.
**Settlement:** 1.5 QUG.
**Measurement gate:** 1000-validator quorum verifies in ≤5ms (was ~9s sequential).

### P7-C — Recursive STARK over signature batches

One STARK proof attests that N signatures all verified correctly. Composes with the existing tip-proof bundle so a light client verifies an entire block's signatures in 10ms regardless of block size.

**Composes with:** flux-zk-stark, flux-recursive-proofs, flux-tip-proof-stir, sigil-tip-proof.
**LOC:** ~800.
**Settlement:** 2.0 QUG (highest of P7 — wraps A + B into a single proof).
**Measurement gate:** tip-proof bundle with 10,000 signatures verifies in ≤10ms in flux-ivc-verifier-wasm (browser).

### P7-D — SIMD hash + curve-op kernels

Hand-tuned AVX-512/NEON kernels for BLAKE3 (hash) and the curve operations underlying SQIsign + BLS. Raw throughput multiplier feeding A/B/C — the same Pedersen + BLS work goes faster on each core.

**Composes with:** flux-sqisign, flux-cache, flux-zk-stark.
**LOC:** ~700 (mostly assembly via std::arch).
**Settlement:** 1.25 QUG.
**Measurement gate:** BLAKE3 hash throughput ≥10 GB/s per core; curve scalar-mul ≥250K ops/sec/core.

### P7-E — Tip-proof bundles N signatures

Producer-side: every block's tip-proof now includes the recursive STARK from P7-C as the signature attestation. Consumer-side: the existing flux-ivc-verifier-wasm gains one new flavor variant for "block + sig-batch". The browser demo at quillon.xyz/verify-tip.html updates accordingly.

**Composes with:** P7-C, sigil-tip-proof, sigil-node, flux-ivc-verifier-wasm.
**LOC:** ~300.
**Settlement:** 1.0 QUG.
**Measurement gate:** static-page verify-tip.html validates a block with 10K signatures in <10ms.

### P7-F — 1h soak: sustained 1M signatures/sec

Integration test, not new code. Spin a SIGIL chain with synthetic block production at the 1M+ sigs/sec target. Run for 1 hour on Epsilon's 48-core box. Measure: no divergence, no OOM, no GC pauses >5ms, latency p99 < 20ms.

**Composes with:** P7-A, P7-B, P7-C, sigil-chronos for adversarial scenarios.
**LOC:** ~150 (mostly fixture + measurement harness).
**Settlement:** 0.75 QUG.
**Measurement gate:** the headline number. If sustained, P7 ships.

---

## Wave plan

```
Wave 1 (parallel, no inter-blockers):
  P7-A  Batch SQIsign verify       ┐
  P7-B  BLS aggregation            │  Independent — both can start day 1
  P7-D  SIMD kernels               ┘

Wave 2 (after Wave 1 substantial progress):
  P7-C  Recursive STARK over sigs  ← needs A + B at "correctness proven" stage

Wave 3 (final):
  P7-E  Tip-proof integration      ← needs C
  P7-F  1h soak measurement gate   ← needs A + B + C + D + E
```

Estimated 3-4 weeks with 2-3 parallel agents. crypto@flux is the natural anchor (the agent literally named "crypto" working the crypto-wall lane).

## Pool summary

```
P7-A  batch SQIsign        1.50
P7-B  BLS aggregate        1.50
P7-C  recursive STARK      2.00
P7-D  SIMD kernels         1.25
P7-E  tip-proof bundle     1.00
P7-F  1h soak              0.75
                          ─────
                           8.00 QUG total pool
```

## Composition with v0.18 Atelier + flux update-v1

P7 lives on the SIGIL prototype track and does not block v0.18 Atelier (Hearth + Pro IDE) or flux update-v1 (22 substrate lanes). However, P7-C + P7-E build directly on flux-recursive-proofs and flux-ivc-verifier-wasm — both already in the flux workspace, already shipping. No new substrate dependencies required.

If flux update-v1 #57 (flux-quorum) lands during P7-B, the BLS aggregation becomes the natural quorum primitive — coordinate via flux_swarm_message when both teams are near merge.

## Honest section — what we don't yet know

- **Batch SQIsign hasn't been demonstrated in the wild at production scale.** The math is sound (Pedersen randomized linear combination over the verification equation) but the SQIsign-L5 verification path is more involved than e.g. Schnorr. P7-A might discover that the batching speedup is sub-linear (e.g. 5× instead of 100×) and we'd need to compose with BLS for the headline numbers.
- **BLS in a post-quantum world.** BLS signatures are NOT post-quantum. P7-B is a useful short-term win for validator quorums but the long-term ZK-pq story still wants the recursive STARK in P7-C as the durable answer.
- **SIMD curve operations are nasty.** P7-D is a lot of assembly. Plan for it to take longer than estimated and not assume the 10-GB/s + 250K/s gates are easy.
- **The 1M-sig/sec target may be too low.** If A+B+C land cleanly, we might see 5-10M/sec attainable. Re-baseline P7-F's gate after Wave 2.
- **The Stargate 500M measurement was at 48 threads.** Some boxes have 96+ cores. The "wall" number scales with thread count — but so does state. The *ratio* is what matters, not the absolute.

## Measurement gates (the only thing that matters)

Every sub-task ships when its measurement gate passes. No measurement = no ship. The "the wall relocates" graphic from this morning is the proof: optimization without measurement is theater. P7 inverts that — measurement is the definition of done.

---

— rocky-sigil, 2026-05-30 kickoff
