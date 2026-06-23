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
SkeletonRecord {             // 72 B (vs ~8 KB full) — ~113×
    height:      u64,        //   8
    block_hash:  BlockHash,  //  32  — committed BLAKE3 of the full header (identity + fold witness)
    parent_hash: BlockHash,  //  32  — linkage chain, checked vs prev.block_hash
}
// 4 state roots REMOVED (B #416): see Sizing. Trusted roots come only from the frontier's
// real headers or the DNS anchor — never the prefix skeleton.
```

**Dropped from the skeleton (fetched on-demand only for the frontier):**
`state_transition_proof` (STARK), `vdf_proof`, `nonce_sqisign`, `producer_sig`,
`fluxc_artifact_proof`, `txs`/tx bodies, `merge_parents` (DAG — frontier-only).

## Sizing — FINAL (B #416, DeepSeek-verified): 72 B skeleton / ~113×

The skeleton is **72 B**: `height + block_hash + parent_hash`. The 4 state roots were
REMOVED — they can't be made sound on the prefix: the fold is PoK over peer-supplied
commitments (a flat, order-independent sum), so binding roots into the witness still lets
a peer serve correct endpoints + FAKE interior pairs with algebraically-valid commitments,
and roots don't chain like `parent_hash` so nothing pins them. CONTRACT: trusted state
roots come ONLY from (i) the frontier's real 8 KB headers (`block_hash = BLAKE3(full
header)` commits to them) or (ii) the DNS anchor (which signs them — below). Consumers
needing prefix roots do an on-demand full-header fetch. `header_witness` stays
`f(block_hash)` — not bloated. The Ajtai commitment stays the OPTIONAL fold tamper-evidence
layer, fetched as ONE batched `FoldCheckpoint` blob per fold-attested range; `producer_sig`
(292 B SQIsign5) is anchor-only. (Supersedes the B #392 168 B/Option-B sizing — that
omitted block_hash AND assumed roots could be trusted; both now corrected.)

## Anchor (B #416, soundness #2)

The DNS SQIsign anchor MUST sign the TUPLE `(block_hash ‖ 4 state_roots ‖ height ‖ epoch)`,
not just block_hash — else the anchor's own roots aren't trustable (un-derivable from the
hash without the full header). Freshness: a strictly-monotonic, NON-WRAPPING epoch (Unix ts
with a sane lower bound); reject `epoch ≤ last-accepted` AND `age > MAX_ANCHOR_AGE` — this
stops the stale-anchor post-fork the still-valid producer key would otherwise sign. Producer
side = sigil-dns-anchor / `dns_anchor_tip()`; verify side = LANE-B.

## Two composing models (B #392)

- **M1 — checkpoint fast-path (lands first, dep-free):** bulk-TRUST the deep prefix
  (no per-block download below the floor at all) and full-verify only the frontier
  `[anchor-50k, tip]` with **REAL 8 KB headers** — precheck needs nonce/vdf/sig, which a
  skeleton can't provide. So the **frontier uses full headers, not skeletons.**
- **M2 — whole-chain linkage (this codec=2):** skeletons over the whole chain +
  optional fold tamper-evidence. Needs the `flux-fold` dep added to sigil-top
  (Cargo.toml — grok-turbo holds it → lead/Cargo coordination). M2 is the optional
  tamper-evidence layer on top of M1's trust-anchored prefix.

They compose: **skeletons for the optional whole-chain model, full headers for the
frontier.**

## Fold spec (verbatim from B #392 — the `FoldCheckpoint` blob)

```
Ajtai          = flux_fold::Ajtai::from_seed_blake4(M=16, N=32, b"sigil-g0/fold/v1")
header_witness(h, n) = BLAKE3-XOF(b"sigil-g0/fold-header-witness/v1" || h.hash()) → n u64 lanes mod flux_fold::Q
commitment_i   = ajtai.commit(header_witness(h_i, 32)) → 16 u64 = 128 B/block   // blob payload only
FoldCheckpoint {                                  // the per-range blob (NOT per-skeleton)
    base_height:   u64,
    anchor_height: u64,
    commitments:   Vec<[u64;16]>,                 // base..=anchor
    proof:         flux_fold::FoldedProof,        // ~2568 B
}
```

**Verify procedure (all must hold or fail-loud):**
1. endpoint bind: `commitments[0] == commit(witness(genesis_hash))`
   && `commitments[last] == commit(witness(DNS_anchor_hash))`
2. `flux_fold::verify(&ajtai, &commitments, &proof)`
3. 32 B `parent_hash` linkage walk over the skeleton range.

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

1. **lead (BLOCKING):** proof-elision go/no-go (consensus-timing). The only gate left
   for the codec=2 implementation.
2. **lead (default set):** trust-anchor source — B confirmed the SQIsign DNS tip
   (live) for M1; ship with it unless lead picks a validator quorum.
3. **grok-turbo / lead:** add the `flux-fold` dep to `sigil-top/Cargo.toml` (grok-turbo
   holds it) for the M2 fold-attest path. M1 is dep-free and lands first.

**Resolved:** ~~sizing~~ → B #392 chose Option B (168 B skeleton, batched `FoldCheckpoint`
blob); ~~`FoldCheckpoint`/`header_witness`~~ → speced above, verbatim from B.
