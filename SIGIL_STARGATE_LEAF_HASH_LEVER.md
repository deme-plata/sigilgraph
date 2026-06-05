# Stargate #1b — the sound fast-leaf-hash lever (measured verdict)

> Filed against PROJECT_STARGATE_v1. Continuation of the BLAKE4 question:
> roots_throughput showed a faster accumulator leaf hash is worth up to
> +144% TPS — but via an UNSAFE non-crypto hash. This measures how much of
> that ceiling is *safely* capturable. — rocky-sigil, 2026-05-30

## Measured (leaf_hash_lever, release, Epsilon)

10k wallets · 1000 blocks × 100 tx. Strategies:
- B per-tx · BLAKE3   (incremental baseline)
- D coalesced · BLAKE3 (SOUND lever — hash once per unique touched key/block)
- C per-tx · fasthash  (UNSAFE ceiling — non-crypto wyhash leaf, for reference)

| workload | B (baseline) | D coalesced (sound) | C fasthash (unsafe ceiling) | D captures of ceiling |
|---|---|---|---|---|
| UNIFORM | 326,433 TPS | 278,315 TPS (**-15%**) | 796,996 TPS (+144%) | 0% |
| HOT (80%→1% hot set) | 331,119 TPS | 417,180 TPS (**+26%**) | 851,548 TPS (+157%) | 17% |

## Verdict (corrected by the data, twice)

1. **Coalescing is modest + adaptive.** It HELPS hot/realistic state (+26%:
   DEX pools, master wallet, popular contracts get touched many times per
   block → fewer unique-key hashes) but HURTS uniform random (-15%: dirty-set
   bookkeeping with nothing to coalesce). **Ship it adaptively** — coalesce
   when touch-locality is high, skip otherwise. Sound, zero audit surface.

2. **There is NO sound algorithmic shortcut to the bulk of the win.** Even
   ideal coalescing captures only 17% of the +144% ceiling. The remaining
   ~125% genuinely requires a faster *per-call* hash. The data is unambiguous.

3. **A faster cryptographic leaf hash is therefore a REAL lane — but a real
   cryptographic-engineering one, not a one-shot prototype.** Hand-rolling an
   AES-rounds / reduced-round-BLAKE3 leaf and calling it "sound" would be an
   overclaim; collision-resistance for an *additive accumulator* (where a leaf
   collision = state forgery) needs a proper construction + review. The two
   defensible candidates, both keeping full security:
   - **SIMD-batched full BLAKE3** — batch the block's dirty leaves and hash
     them many-at-once via BLAKE3's parallel path. Same hash, amortized
     per-call overhead + SIMD width. Needs a small accumulator refactor
     (defer + batch). The sound way to chase the ceiling.
   - **A vetted faster CRHF** under flux-eternal-cypher's crypto-agility
     layer — swappable, benchmarked, cryptanalysis-gated before it ships.

4. **The dominant lever is Stargate #2, not the hash.** Post-roots, execution
   is 99.9% of wall-clock. 48 cores × the ~330k single-thread rate clears 1M
   TPS without touching the leaf hash at all. Parallel execution (Block-STM)
   is the bigger, simpler multiplier. The hash lane is a +26% (sound, now) to
   +125% (needs crypto work) refinement on top.

## Recommendation

- **Now (sound, free):** adaptive coalescing for hot keys.
- **Next (sound, small refactor):** SIMD-batched BLAKE3 — measure how close
  it gets to the ceiling without weakening the hash.
- **Don't:** hand-roll a fast non-crypto or unreviewed reduced-round leaf and
  call it production. The additive accumulator makes leaf collisions fatal.
- **Biggest lever:** Stargate #2 parallel execution — execution dominates.

— rocky-sigil 🟣
