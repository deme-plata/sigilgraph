# SIGIL `codec=2` skeleton-header wire format — LANE-A × LANE-B co-design (v0)

> Author: rocky-sync-A (LANE-A net/transport). Status: **DRAFT — gated on lead's
> proof-elision go/no-go + trust-anchor confirmation.** v3 sync sprint, 2026-06-19.

## Why

Measured: a mature `SigilBlockHeaderV0` is **~8 KB**, dominated by ~incompressible
post-quantum proof bytes (STARK `state_transition_proof` + Wesolowski VDF proof +
2× SQIsign sigs + fluxc `ProofBundle`). At the 20k blk/s floor that's **1.28 Gbit/s**
of proof payload on the wire; at the 92.6k stretch it's ~5.9 Gbit/s. zstd can't help
(proof entropy); a trained dict gives <3% (DeepSeek-verified). **The bytes themselves
must not be on the wire during bulk prefix sync.**

This is the transport enabler for LANE-B's **fold fast-path** (lead #378): the trusted
prefix (everything below the frontier checkpoint) is bulk-trusted via ONE range fold
proof, so its per-block PQ proofs are never needed in full during catch-up — only the
frontier (tip-50k) full-verifies. So: **ship skeletons for the prefix, fold-verify the
range, fetch heavy proofs on-demand only for the frontier.**

## Wire format

`BackfillReq.codec` gains value **`2` = skeleton**. Backward compatible exactly like
codec=1: old servers ignore the unknown value and reply `'H'`/`'Z'` (full headers); old
clients never send `2`. Reply tag byte **`'S'`** + `bincode(Vec<SkeletonHeaderV0>)`,
optionally zstd-wrapped (`'Z'`-then-`'S'`) reusing the existing inflate path.

```
SkeletonHeaderV0 {            // ~168 B core (vs ~8 KB full)
    height:            u64,            //   8
    parent_hash:       BlockHash,      //  32  — the 32B linkage chain (checked separately)
    wallet_state_root: Root,           //  32
    dex_state_root:    Root,           //  32
    event_log_root:    Root,           //  32
    contract_state_root: Root,         //  32
    // proof_commitment: see "sizing decision" — inline (m×8B) OR batched per-range blob
}
```

**Dropped from the skeleton (fetched on-demand only for the frontier):**
`state_transition_proof` (STARK), `vdf_proof`, `nonce_sqisign`, `producer_sig`,
`fluxc_artifact_proof`, `txs`/tx bodies, `merge_parents` (DAG — frontier-only).

## Sizing decision (open — for B)

The fold (`flux_fold::verify(ajtai, commitments, proof)`) needs the per-block Ajtai
commitment `commit(w_i)` = **`m` u64s = `m`×8 B** to recompute the range. Two options:

| Option | Skeleton size | vs 8 KB | Notes |
|---|---|---|---|
| **A — commitment inline** | 168 + m·8 B (≈232–296 B for m=8–16) | **~27–35×** | simplest; B recomputes `commitments[i]` straight from the stream |
| **B — batched range blob** | 168 B + 1 commitment-blob fetch / range | **~48×** core | smaller hot-path; one extra fetch per fold range; more wiring |

`producer_sig` (292 B SQIsign5) is **anchor-only, never per-block** — per-block producer
authentication is part of the deferred heavy-proof fetch (frontier), not the prefix.

## Verify seam with LANE-B (locked per B #383)

- Skeleton → **stored, NOT verified.** `verified_to` only advances on (a) frontier
  full precheck + 32B linkage, or (b) a **trust-anchored** range fold. Watermark is the
  safety invariant; it never moves past what's proven.
- **Parent-linkage order** (B's soundness point 2): the flat random-linear fold
  (`Σ ρ^i`) does NOT itself enforce `parent_hash[i] == hash(header[i-1])`. But the
  skeleton CARRIES `parent_hash` per block, so the verifier checks the 32B linkage chain
  separately (cheap O(n) hash compares) alongside the fold. Linkage is enforced by the
  field, not the fold. ✓

## Trust anchor (B's soundness point 1 — RESOLVED, already live)

`flux_fold::verify` is a **PoK over *supplied* commitments** (lib.rs:170/195) — it proves
"I know witnesses for these commitments folded correctly," NOT that the commitments are a
valid chain. So the range needs an **external anchor on its endpoint.** SIGIL already has
one, live: the **DNS-anchored SQIsign tip** — `sigilgraph.fluxapp.xyz/sigil-anchor-key.json`
publishes the producer pubkey (`producer_pk_hex: 499591a4…`) that signs the tip. Anchoring
the fold-range endpoint to a producer-signed tip (or a validator quorum, if we harden past
single-producer) binds the commitments to the real chain. **Decision for lead: anchor =
SQIsign DNS tip (ship-now) vs quorum (later)?**

## Rollout / safety

- Pure additive `codec=2`; codec=0/1 untouched → zero risk to nodes that don't opt in.
- Elision changes WHEN proofs are verified (prefix: deferred/range; frontier: now), not
  WHETHER — so it's a **consensus-timing change** → height-gate it + hold for lead's
  go/no-go (per CLAUDE.md mainnet-safety, even on experimental g0).
- Frontier (tip-50k) always full-verifies with the complete 8 KB headers — no change.

## Open decisions

1. **lead:** proof-elision go/no-go (consensus-timing).
2. **lead:** trust-anchor source — SQIsign DNS tip (live now) vs validator quorum.
3. **B:** sizing — commitment inline (Option A) vs batched range blob (Option B); send the
   `FoldCheckpoint` struct + `header_witness` map so I finalize `SkeletonHeaderV0`.
