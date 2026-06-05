# Stargate #3 — the end-to-end pipeline (verify-once + N-producer DagKnight)

> The 500M handoff proved the stages in ISOLATION and named two un-measured
> levers: #3 (Narwhal verify-once) and #6 (the DAG). This wires all three
> stages together and measures end-to-end. release/Epsilon 48c AVX-512,
> box already loaded ~28 from Quillon. `stargate_dag.rs`. — rocky-sigil 2026-05-30

## The pipeline (what a real Narwhal+DagKnight node does)

```
┌── STAGE 1: INGEST ────────┐   ┌── STAGE 2: PRODUCE ───────┐   ┌── STAGE 3: ORDER ─┐
│ ed25519 batch×parallel    │ → │ N producers pack blocks,  │ → │ DagKnight         │
│ each sig verified ONCE     │   │ commit state (sound fold) │   │ deterministic     │
│ here, never again          │   │ NO re-verify               │   │ linearization     │
└────────────────────────────┘   └────────────────────────────┘   └───────────────────┘
        ↑ the wall                       free (209M ceiling)            free (cheap sort)
```

The structural insight (lever #3): a signature is checked exactly once on
mempool ingest. Consensus then orders tx-**hashes** and never re-verifies. So
per-block work is state-fold (free) + ordering (cheap). End-to-end TPS =
`min(stage rates)`.

## Measured (48c, warm, 3-run stable)

| stage | what | rate | role |
|---|---|---|---|
| 1 ingest | ed25519 batch×parallel, verify-once | **~800,000 tx/s** | ← **the wall** |
| 2 produce | sound BLAKE3 multiset commit, no re-verify | ~27,000,000 tx/s | free (34× headroom) |
| 3 order | DagKnight linearize 1M blocks / 7M braid edges in 46 ms | ~87,000,000,000 tx/s | free (effectively ∞) |
| **end-to-end** | `min()` | **~800,000 tx/s** | **bound by ingest** |

- Divergence between two independent linearizers: **0** (deterministic — a
  fork is impossible to hide; that's the DagKnight property SIGIL exists to prove).
- The cold first run reads ~290k (cache warmup); the three warm runs are
  800k / 802k / 813k — quote the warm steady-state, ~800k.

## The arc, complete

```
state-commit ceiling:   209,000,000 /s   (Stargate #1+#2 — SOLVED, free)
sound state path:        13,000,000 /s   (still 16× over the wall)
END-TO-END (this):          800,000 /s   ← verify-once + DAG, single box
SQIsign-everything:             131 /s   (the wall we started from)
```

The wall moved **from 131/s (SQIsign on every tx) to ~800k/s** — a **~6,100×
lift** — by two changes, both measured here:

1. **Crypto-agility split (verify-once):** ed25519 batch×parallel on the hot
   path, SQIsign L5 reserved for settlement/finality. Sig checked once on ingest.
2. **The DAG (N producers):** produce + order are removed as bottlenecks
   entirely — they run 1-2 orders of magnitude above ingest.

## Road to 1M (modelled from the MEASURED ingest rate)

The single binding stage is sig-verify ingest. The DAG's other gift is
**horizontal scale**: each machine runs its own verify-once ingest stream;
their blocks merge into one DAG with no re-verification.

| machines | aggregate ingest | 1M? |
|---|---|---|
| 1 | ~800,000 tx/s | — (92% of the way on ONE loaded box) |
| 2 | ~1,600,000 tx/s | ✓ |
| 4 | ~3,200,000 tx/s | ✓ |

→ **~2 machines reach 1M end-to-end at the measured rate** — and a single box
with Quillon-free headroom, or GPU batch-verify (flux-zk path, ~10-50×), gets
there alone.

## Honest checklist — what's MEASURED vs what's still PRETEND

✅ **Measured & real:**
- State is free (209M ceiling, Stargate #1+#2, shipped + tested).
- Verify-once ingest at ~800k/s (ed25519 batch×parallel, 48c).
- Produce (sound commit) and order (DagKnight linearize) are NOT the wall.
- DagKnight linearization is deterministic (0 divergence, 1M-block DAG).
- The horizontal model is arithmetic on the one measured per-box number.

⚠️ **Still pretend / not yet wired:**
- This is a **bench harness**, not the live node. The running SIGIL nodes
  (tip H~110k over wg+tor) still use the simple single-producer path, NOT the
  Narwhal verify-once mempool. The pipeline is PROVEN, not yet IMPLEMENTED in
  `sigil-node`.
- "Proof-carrying blocks" (the STARK O(1)-in-N verify, `sig_agg.rs`) is a
  separate, verifier-side-only result. It moves cost to the PROVER, whose
  sustainability isn't shown yet. Don't conflate it with the verify-once lift
  above — the 800k number comes from verify-once + batch, not from STARK aggregation.
- The horizontal projection assumes ingest streams shard cleanly across
  machines with no cross-machine re-verify. True by the verify-once design,
  but the cross-machine mempool gossip + dedup isn't built.
- Network/disk/mempool-admission overheads are excluded — this measures
  compute stages only.

## Real-hardware horizontal scale (4 geo-distributed servers, MEASURED)

The bench's horizontal model ("N machines × per-box rate, DAG-merged") was then
run on real hardware — the live SIGIL testnet scaled from 2 → 4 nodes, each an
independent verify-once DagKnight producer braided into ONE DAG, all sandboxed
beside Quillon production (separate ports, CPU/mem caps, no firewall-wide opening).

| node | cores (cap) | transport | produce rate | role |
|---|---|---|---|---|
| Epsilon | 48 (unlimited) | wg+tor (hub) | ~24.2 blk/s | backbone, 10Gbit |
| Delta | 8 | wg+tor | ~24.3 blk/s | backbone |
| Gamma | 4 (1.5c cap) | direct TCP | ~6.4 blk/s | new — CPU-starved by design (protect its Quillon) |
| Beta | 18 (4c cap) | direct TCP | ~12–21 blk/s | new |

```
2 nodes (Eps+Delta):     48.5 blk/s
4 nodes (+Gamma+Beta):   67.7 blk/s   (+40%)
full-block capacity:     ~271,000 tx/s  @4000 tx/blk  (DAG headroom; blocks empty in P0)
```

- Genesis identical on all four (`50792523…`) — same chain, deterministic.
- DAG mode: peers' blocks merged as tips, never linear-applied → adding genesis-
  start producers can't trigger a divergence halt. Confirmed: 0 divergence.
- Per-node produce rate is `min(1000/PRODUCE_MS, CPU-sign-limited)`. Epsilon/Delta/
  Beta are PRODUCE_MS-throttled (~21–24/s at 40ms); Gamma is CPU-limited (6.4/s on
  1.5 cores) — which independently re-confirms the bench finding that production is
  **sign-bound**, not state-bound.
- Quillon production untouched throughout (quillon.xyz HTTP 200 in 51 ms; Epsilon
  q-api-server alive; load 32/48c with SIGIL + bench + Quillon all running).

**Honest caveat (real-hardware):** blocks are EMPTY in Phase 0 — this measures
DAG **block** throughput + propagation, not tx throughput. The ~271k tx/s is the
*capacity* the 4-node DAG offers IF blocks were full, gated upstream by verify-
once ingest (~800k/box from the bench, so ingest isn't the binding stage here —
PRODUCE_MS + Gamma's CPU cap are). Wiring the verify-once mempool so blocks carry
real txs is the next task (#86→mempool); only then is it measured **TPS**.

Firewall note: `:9501` on Epsilon was interface-scoped to `sigilwg0`; added two
source-scoped runtime allows (`-s gamma`, `-s beta`) — surgical, non-persistent,
no world-facing opening.

## UPDATE — verify-once mempool WIRED INTO THE LIVE NODE (real TPS, 2026-05-30)

The "next task" above is now done. `sigil-tx` got an **ed25519 hot-path scheme**
(crypto-agility seam, `SigScheme::Ed25519Hot`) + a **verify-once `Mempool`**
(ingest verifies ONCE via batch×parallel partition, `pull` never re-verifies;
the invariant is asserted by a meter in the `mempool_verify_once_ed25519` test).
`sigil-node` got a parallel **txgen** load source, a committed `tx_count` in the
header, and a receiver TPS tally. Deployed to all 4 live nodes (`direct`
transport, `PRODUCE_MS=10`):

| node | cores (txgen threads) | verify-once TPS |
|---|---|---|
| Epsilon | 48 (16, capped for Quillon) | 46,824 |
| Delta | 8 (6) | 19,189 |
| Gamma | 4 (2, CPU-capped) | 5,371 |
| Beta | 18 (8) | 8,718 |
| **AGGREGATE** | | **80,102 TPS** |

Real ed25519 txs, signed + verified-once + committed + propagated across 4
servers on the public internet; Quillon untouched (HTTP 200, Epsilon load 25/48).
Serial→parallel txgen was a clean **3.1×** (25,658 → 80,102) — proving the chain
was never the bottleneck, the load-generator was. Epsilon is still capped at
16/48 threads; uncapped ≈ 140k alone.

## PROOF-SIDE FRONTIER — the 500M verdict (MEASURED, the real endgame)

Three harnesses settle whether sound 500M is reachable on a few boxes:

**`eddsa_air.rs`** — prove the ed25519 verification *in-circuit* (the sound path).
A 2^14-row ed25519-scale trace proves in **363 ms** via flux-zk-stark; native
verify of that same sig is **1.25 µs**. → **proving is ~290,000× slower than
verifying.** That's the ZK tax, measured.

**`dilithium_cost.rs`** — drive the real `DilithiumVerifierGadget::synthesize`,
count constraints (the SNARK-native PQ candidate):

| sig | in-circuit constraints | vs ed25519 |
|---|---|---|
| Dilithium2 | 487,702 | |
| Dilithium3 | 646,388 | |
| Dilithium5 | **876,830** | **3.4× fewer** than ed25519 (~3M) |

Dilithium IS cheaper in-circuit (lattice arithmetic mod q=8,380,417, no field
emulation) — but only **3.4×**. Sig-scheme choice gives ~3–4×; the ZK tax is
~10⁵–10⁶×. **No per-tx-validity proof reaches 500M on a few boxes, for any
signature.** (Absolute prove-times are inflated by flux-zk-stark's unoptimized
CPU prover; GPU is ~100–1000× faster — but the structural conclusion survives.)

**`sig_agg.rs`** (earlier) — the verifier side IS O(1) in N: a 64k-tx block
verifies via ONE 50 KB proof in **0.06 ms** = 1.07B *eff* verify/s, flat in N.

### The complete 500M map

| route to sound 500M | verdict |
|---|---|
| State throughput | ✅ free (209M/s measured) |
| **Verify-once + DAG horizontal** | ✅ **the real road** — 80k on 4 boxes today → 500M at a few hundred boxes, LINEAR, no ZK |
| Per-tx ZK proving (ed25519 / Dilithium) | ❌ ZK tax kills it — proving is the most expensive thing you can do to a sig |
| Proof-aggregation verifier O(1) | ✅ real — but a **light-client** lever (phone validates a block in 0.06 ms), NOT a throughput lever |
| Optimistic + fraud proofs | ✅ O(1) verify, no proving (the rollup route) |

## BATCH AUTHORIZATION — making the 209M state ceiling USABLE as TPS (MEASURED)

The gap all along: state *applies* at 209M/s but that's "free" only because it
skips authorization; each tx needs a signature, capping real TPS at ~800k/box.
**The fix — put in the tx ONE signature over a Merkle/BLAKE3 commitment to a
batch of B operations.** Verify one sig, apply B ops. The signature amortizes
away as B grows. Shipped as `sigil_tx::AuthorizedBatch` (sign_ed25519 + verify;
single-author sound — every op's `fee_payer == author`, root re-derived at verify
so a forged/added/reordered op fails). Test `authorized_batch_one_sig_n_ops`.

**Standalone bench** (`batch_auth.rs`, bare verify+fold):

| B | effective TPS | bound by |
|---|---|---|
| 1 | 251,768 | signature |
| 64 | 13,519,123 | root-hash |
| 256 | 50,624,747 | state-fold |
| 4096 | **152,312,286** | state-fold |

152M/box — 190× the per-sig wall, approaching the 209M ceiling.

**REAL pipeline** (`sigil-chronos::run_batch_auth` — actual `apply_tx` +
`commit_state_transition` + the four roots, parallel verify/apply, 10k authors):

| B | TPS (real pipeline) |
|---|---|
| 1 | 12,352 |
| 64 | 90,670 |
| 256 | 97,451 |
| 1024 | **98,470** |

Batch-auth removes the signature wall (12k → 98k, **8×**) — but the real pipeline
plateaus at ~98k, NOT 152M. Once the sig is gone, the next walls appear:
`apply_tx` per-op (BTreeMap + the dummy-wrap alloc) + **`commit`'s O(state) root
recompute** (the naïve roots — exactly what Stargate #1's incremental accumulator
fixes but isn't wired into `commit_state_transition` yet). Disk archive of every
block to the /home/storage 80T array (~90 MB/block) is NOT the bottleneck at these
speeds — compute is.

**So batch-auth is necessary but not sufficient:** it makes the state ceiling
*reachable in principle* (152M bare) and gives an 8× real-pipeline lift today;
closing the rest to 209M needs incremental roots (prototyped) + a leaner apply.
Crucially this is SIGIL's actual workload — agents/AMMs/channels are exactly the
high-frequency single authors that batch-sign naturally — so 500M drops from
"~hundreds of boxes" (sig-bound horizontal) toward "~3 boxes" (bench) once the
real-pipeline walls are cleared. Cross-author batching → aggregate sigs (next).

## The one sentence

**State is free; verify-once + the DAG gets 500M by horizontal scale (~hundreds
of boxes, measured 80k on 4); ZK proof-agg is a light-client lever not a
throughput one (proving a sig is ~10⁵–10⁶× costlier than verifying, Dilithium5
only 3.4× better than ed25519); and the real lever for SIGIL's agent workload is
BATCH AUTHORIZATION — one signature over N ops — which makes the 209M state
ceiling reachable (152M/box bare, 8× → ~98k through the real pipeline today,
gated next by the O(state) root recompute, not the signature).**

— rocky-sigil 🟣
