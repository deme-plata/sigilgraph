# Quillon/Flux privacy-crypto audit — what's real, what's theater, and the SIGIL path

Date: 2026-07-21. Trees read: `/home/orobit/q-narwhalknight-src` (Quillon),
`/home/storage/deepseek-codewhale/flux` (Flux). Goal: understand why private
transactions never actually worked on Quillon, and choose the honest foundation
for SIGIL privacy.

## TL;DR verdict

| Crate | Post-quantum? | Real proving? | Wired to tx path? | Verdict |
|---|---|---|---|---|
| `q-zk-snark` (Groth16/bn254) | ❌ no | groth16.rs REAL; **tx-privacy path FAKE** | ❌ not called | Theater for private txns |
| `q-zk-stark` (Quillon) | ✅ (hash-based) | ❌ FRI zero-filled | via API stub | **Fake STARK** |
| `flux-zk-stark` (Flux) | ✅ (hash-based) | ❌ same zero-filled FRI (port of the above) | no | **Fake STARK** |
| `q-crypto-advanced` | ✅ (winterfell/bulletproofs) | bulletproofs REAL; Circle-STARK thin | no | Partial real primitives |
| **`flux-lattice-guard` (LatticeGuard)** | ✅ **lattice/RLWE** | ✅ **base proof REAL**; folding = Phase-C scaffold | no | **The real foundation** |

**Why private txns never succeeded:** the SNARK "transaction privacy proof" was a
placeholder, and the STARK's FRI was zero-filled — there was never a real proof in
the money path.

## Evidence

### 1. The SNARK "transaction privacy proof" is a placeholder
`q-zk-snark/src/wallet_privacy.rs :: prove_transaction_privacy` builds a real-looking
circuit (arkworks R1CS gadgets, `range_proof`), then:
```rust
let circuit = builder.build();      // ← built and discarded
Ok(TransactionPrivacyProof {
    proof: vec![0u8; 128],          // ← 128 ZERO BYTES, no prover ever ran
    ...
})
```
`verify_transaction_privacy` accepts any 128-byte blob (`Ok(true)`), comment: "Real
implementation would check against nullifier set." The **real** groth16 prover DOES
exist (`groth16.rs:107` calls `Groth16::<E>::prove`) — it's just never used by the
privacy path.

### 2. It isn't even called from the transaction flow
`grep prove_transaction_privacy|WalletPrivacyProver` across `q-api-server/src` and
`q-dex/src` → **zero hits**. The only API surface (`q-api-server/src/zk_proof_api.rs:324`)
is itself stubbed: `// For now, create a placeholder proof; let proof_bytes = vec![0u8; 200]`.

### 3. Both "STARK" crates fake the FRI
`q-zk-stark/src/batch_prover.rs:345` and the **identical** `flux-zk-stark/src/batch_prover.rs:345`
(`generate_fri_proof`) fill the layers with `extend_from_slice(&[0u8; 32])` /
`vec![0u8; current_size * 8]` / `vec![0u8; 16*256]`. Deps are only `sha3`+`blake3` — no
`winterfell`/`plonky3`. `stark_prover.rs` claims "Production-grade … Real FRI" in its
doc comment but `compute_trace_commitment` is "Simplified commitment - hash the trace".
These are not STARKs.

### 4. The genuinely real primitives
- **`q-crypto-advanced`**: real `winterfell 0.9` dep; `bulletproofs_v2.rs:251 prove()`
  is a real Bulletproofs range proof (not PQ, but real). Circle-STARK has error types
  but thin winterfell wiring.
- **`flux-lattice-guard` (LatticeGuard, "Novel Lattice-Based Post-Quantum zk-SNARK")** —
  4,052 LOC, and the base proof is REAL:
  - `prover.rs::generate_proof`: real `NttOperator`, `PolynomialCommitment::commit`,
    `ApproximateProductProver`, evaluations at Fiat-Shamir challenge points.
  - `verifier.rs::verify`: reconstructs challenges via Fiat-Shamir, checks polynomial
    evaluations against an **error bound** (`error {} > bound {}`), verifies approximate
    product proofs, evaluates public linear combinations. It genuinely recomputes and
    rejects.
  - **Honest gap:** `folding.rs` is explicitly "Phase C scaffold — Module-SIS folding
    (LatticeFold/LaBRADOR)" with placeholder constructors; `approximate_product.rs:362`
    aggregation is "simplified." So the single-proof is real; recursion/folding is not
    finished.

## Recommendation for SIGIL

Do NOT port the STARK crates — they're zero-fill fakes in both trees. Two honest routes,
both post-quantum:

1. **LatticeGuard finish (highest leverage):** build SIGIL private transfers on
   `flux-lattice-guard`'s real base proof (RLWE + approximate products), which is already
   post-quantum. Scope = a shielded-transfer circuit (Pedersen-style lattice commitment to
   amount + nullifier + Merkle membership) proved by the existing `generate_proof`, verified
   by the existing `verify`. Defer folding/recursion (Phase C) — a single non-recursive proof
   per transfer works without it.
2. **Real STARK via winterfell:** if a transparent hash-based STARK is preferred, use the
   REAL `winterfell 0.9` already vendored in `q-crypto-advanced` (define an `Air` + `Prover`)
   rather than the fake `*-zk-stark` crates. More code, but no lattice-parameter risk.

Either way the non-negotiables that were missing before: (a) the proof must be produced by
a real prover and (b) `verify` must reject a bad/zeroed proof (test it by tampering), and
(c) it must be wired into the actual transfer path with a persistent **nullifier set** to
stop double-spends. The prior code failed all three.

**Parameter caveat (LatticeGuard):** `folding.rs` marks its security levels "provisional /
estimated, pending real run." Any lattice route needs the RLWE params pinned to a stated,
externally-checked hardness estimate before it guards real value.
