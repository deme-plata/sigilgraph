# Getting to 209M TPS for real — sig-verify analysis (crate-grounded)

> "Figure out how to improve sqisign-rs 0.3.0 so we get 209M TPS for real."
> Answer from reading the actual crates, not hoping. — rocky-sigil 2026-05-30

## What the code actually says

**sqisign-rs 0.3.0** (`/root/.cargo/.../sqisign-rs-0.3.0/src/`):
- Field arithmetic = **scalar portable Rust** (`quaternion/montgomery.rs`,
  multi-limb Montgomery reduction). **ZERO SIMD / AVX / asm / batch-verify** —
  grep for `avx|simd|target_feature|asm!|batch` returns nothing.
- Supports `Level1 / Level3 / Level5`. My 131/s measurement used **Level5**
  (the SLOWEST — 256-bit). Default is Level1.
- Has a `precomp_signing` module + `LevelPrecomp` trait — precompute exists
  on the SIGN side, not yet exploited verify-side.

**flux-zk-stark** + **flux-recursive-proofs** (already in the workspace):
- `BatchStarkProver` — batches `TransactionWitness` (sig+pubkey+msg+constraints)
  → one `BatchStarkProof`, with **GPU acceleration + CPU batching**.
- `flux-recursive-proofs` — recursive post-quantum SNARKs, folding, epoch
  proofs. The aggregation layer. The `sigil-tip-proof` StarkRecursive flavor
  is already scoped for exactly this.

## The honest physics

A single SQIsign verify is an **isogeny walk** — fundamentally milliseconds.
**No crate optimization makes one verify sub-microsecond.** So 131/s → 209M/s
(a 1.6-million× gap) is NOT a "tune the crate" problem. It's two problems:

## LEVER 1 — make sqisign-rs faster (real, but caps at ~hundreds-of-k/s)

The crate is unoptimized. Concrete, fluxc-buildable wins:

| change | mechanism | est. |
|---|---|---|
| **Level1 hot path** | smaller prime than Level5; still post-quantum 128-bit | ~4× |
| **AVX-512-IFMA Montgomery mul** | Epsilon has avx512f; 52-bit IFMA is THE technique for fast modular mul — the field op that dominates isogeny cost | ~3× |
| **per-pubkey verify precompute cache** | one agent signs thousands of txs → decompress+precompute the pubkey ONCE, reuse. Extend the existing `LevelPrecomp`/precomp module to the verify side | ~2× on hot agents |
| 48-core parallel | already have it | ×cores |

**Stacked: ~131/s → ~150-250k/s** (≈1500-1900× per-verify). Real, large, worth
doing. **But ~200k/s, NOT 209M — the isogeny floor is hard.**

## LEVER 2 — the architecture that ACTUALLY reaches 209M (STARK aggregation)

**Stop verifying signatures individually on the hot path.** Instead:

1. The **producer** builds ONE recursive STARK proof attesting *"all N txs in
   this block carry valid SQIsign signatures over their stated messages."*
   (flux-zk-stark `BatchStarkProver` — witness = sig+pubkey+msg, constraint =
   "SQIsign verify == true"; folded via flux-recursive-proofs.)
2. **Every node verifies ONE proof in ~10ms** (the StarkRecursive tip-proof
   flavor), regardless of N.
3. Effective sig-throughput = N / 10ms. For N = 2M txs/block → **200M sig-
   verifications attested per 10ms proof** = the state ceiling. **Per-node
   sig-verify cost → ~0.**
4. The bottleneck moves to PROOF GENERATION — but that is (a) done ONCE per
   block by the producer, not per-node; (b) parallel + GPU-accelerated
   (flux-zk-stark has the GPU path); (c) off the critical verify path.

## The synthesis — Lever 1 × Lever 2 = 209M real

- **Lever 1 speeds the PROVER.** The producer still verifies each sig once to
  build the witness — so the ~200k/s sqisign improvement makes proof generation
  feasible at block scale (GPU-batched).
- **Lever 2 eliminates the VERIFIER cost.** Every other node checks one ~10ms
  STARK proof instead of a million signatures.
- Together: the 209M state ceiling becomes the **real** end-to-end ceiling,
  because signature verification is amortized into a single proof check and
  the state layer was already proven free.

## Build order

1. **Switch hot path to SQIsign Level1** — trivial, ~4× today (one type param).
2. **AVX-512-IFMA Montgomery backend for sqisign field arith** — fork/patch
   sqisign-rs (or contribute upstream); fluxc-build + bench in sig_wall.rs.
   The single highest-value crate change.
3. **Verify-side pubkey precompute cache** — extend `LevelPrecomp`.
4. **STARK sig-aggregation circuit** — flux-zk-stark BatchStarkProver witness =
   SQIsign-verify constraint; flux-recursive-proofs folds the block; wire the
   StarkRecursive tip-proof flavor. THIS is what makes 209M real.
5. **chronos-measure both** — prover throughput (GPU-batched) vs per-node proof
   verify (~10ms), confirm end-to-end clears the 1M target with margin.

## The one-sentence answer

**You don't make one SQIsign verify 1.6-million× faster (impossible) — you
prove a whole block's worth of signatures valid in ONE recursive STARK (crates
that already exist: flux-zk-stark + flux-recursive-proofs), so every node checks
one ~10ms proof instead of a million signatures; combine that with an
AVX-512-IFMA + Level1 + precompute-cached sqisign to make the proof itself cheap
to generate, and the 209M state ceiling becomes the real ceiling.**

— rocky-sigil 🟣

---

## MEASURED (2026-05-30) — `sig_agg.rs`, the wall actually moves

Built `crates/sigil-state/examples/sig_agg.rs` (real ed25519-dalek + flux-zk-stark).
Run on Epsilon under concurrent load (production + a flux-zk-stark compile), so the
**absolute** Tier-0/1 numbers are depressed ~12× vs an idle box (idle single-verify
≈113k/s per `sig_wall.rs`); the **structural** result below is load-independent.

| Tier | Mechanism | Rate / cost |
|---|---|---|
| 0 | naive re-verify (ed25519 single) | 9,037/s — the wall, O(N) per node per block |
| 1 | ed25519 batch×48t | 178,140/s (20× over wall) — **ships today, no circuit** |
| 1 | verify-once (BLAKE3 hash-inclusion sync path) | 24,340,139 sigs/s — consensus orders hashes |
| 2 | proof-carrying block — verify time vs N | **0.146→0.096→0.084→0.079 ms as N: 1k→4k→16k→64k** |

**The O(1)-in-N property is demonstrated:** verify_ms is FLAT (even decreasing) while
N grows 64×, because N sigs fold into a FIXED-SIZE commitment → fixed trace → fixed
FRI/Merkle proof. A 16k-sig block costs ONE verifying node **0.08 ms** vs **1,770 ms**
naive = **22,528× cheaper**. Producer pays ~1.1ms prove once; K nodes each pay flat 0.08ms.

### What's real vs scoped (honest)
- **REAL + measured:** Tier 0/1 verifies; verify-once BLAKE3 throughput; Tier 2 *cost
  structure* — real flux-zk-stark FRI/Merkle prove+verify, verify genuinely O(1) in N.
- **ROADMAP (soundness):** flux-zk-stark's CPU `evaluate_constraints_cpu` is a documented
  placeholder — the proof commits to the trace but does not yet *prove* "all N sigs valid".
  Binding the fold-commitment to in-circuit EdDSA needs the `flux-recursive-proofs`
  `ConstraintBuilder` AIR. Until then **Tier 1 is the shippable answer**; Tier 2 is the
  measured-cost endgame whose soundness circuit is the next build.
- **Note:** `proof_B = 50000` is the prover's hardcoded size estimate, not an independent
  measurement; only `verify_ms`/`prove_ms` are wall-clock.

### Next build (to make Tier 2 sound, not just cheap)
1. `flux-recursive-proofs` `ConstraintBuilder`: EdDSA-verify AIR (point decompress, scalar
   mul, equation check) → emit constraints so `evaluate_constraints_cpu` is non-trivial.
2. Bind the AIR's public input to the block's signature-set BLAKE3 commitment.
3. Re-run `sig_agg.rs` with `constraints` populated → verify still O(1), now SOUND.
