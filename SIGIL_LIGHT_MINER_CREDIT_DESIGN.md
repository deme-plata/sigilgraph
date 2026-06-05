# Light-miner wallet credit — design (balance-integrity-first)

> Fixes honest-limit #3: the light-miner shows **estimated** earnings (`attested × 0.0005`); this design turns that into **real, committed SIGIL credit** — without repeating the Quillon balance-corruption incident. **No balance-touching code lands until this is signed off.**

## Goal
A light verifier (`sigil-top --mine` / `flux-miner::light`) that verifies the real tip and attests should **earn real SIGIL** from the protocol's operator pool — credited to its wallet, provably, once per unit of honest work.

## The flow
```
light-miner: verify tip (SQIsign/Blake3) ──► build Attestation{height, tip_fingerprint, verifier_wallet, sig}
        │  submit (signed) to node RPC
        ▼
sigil-node: validate ──► (1) tip_fingerprint matches the node's own block at `height`
        │               (2) verifier signature is valid
        │               (3) NOT already credited for (verifier, height)  ← idempotency
        ▼
sigil-bank: credit ──► move `reward` from the 0.1% operator pool → verifier wallet
        │               as a committed StateMutation::Credit (enters wallet_state_root)
        ▼
balance is now PROVABLE — the tiny node can verify its own earnings against the root
```

## The balance-integrity checklist (binding — from the Quillon post-mortems)
1. **Additive deltas, committed in the root.** Credits are `StateMutation::Credit{wallet, amount}` applied through `apply_tx` → they enter `wallet_state_root`. **Never** write a balance outside the committed-state path (Quillon's fatal pattern was uncommitted balances + a batch overwrite). There is no `save_wallet_balances`-style blind overwrite here.
2. **Idempotency = no double-credit.** Key each credit by `(verifier_wallet, height)` (or attestation hash). A second attestation for the same key is a no-op. This is the single most important rule — without it, replay = infinite mint.
3. **Conservation.** A block's total verifier credits **must not exceed** that block's operator-pool allocation (the 0.1% pool emission). Credits are *transfers from the pool*, not new mint. Assert `sum(credits) ≤ pool_balance_for_block`; refuse the surplus.
4. **Gate on real verification.** Credit only a *valid* attestation: the `tip_fingerprint` must equal the node's own committed block at that height, and the verifier's signature must check. An unverifiable attestation earns nothing (it is dropped, loudly).
5. **Sybil/grief bound.** One wallet spinning 1,000 light-miners must not multiply earnings. Rate-limit to **one credit per (verifier, height)** and cap distinct verifiers credited per block; the pool is split, not duplicated. (Open question: per-verifier cap vs. even split — see below.)
6. **Test on isolated state FIRST.** Wire it in `sigil-chronos` (the deterministic sim) over fabricated attestations — prove conservation + idempotency + sybil bound across fuzzed scenarios — *before* any code touches a live node. The `acc_bench`/property-fuzz harness already exists for exactly this.
7. **Genesis-node guard.** No credit path may ever lower a balance; the operator pool is the only debit, and it cannot go negative.

## Where it lands (crates)
- `sigil-events` — `SigilEvent::VerifierAttested` (already has the shielded-send precedent for new tags).
- `sigil-state` — `StateMutation::Credit{from_pool, to_wallet, amount, idem_key}`, applied in the chokepoint, committed to `wallet_state_root`.
- `sigil-bank` — the operator-pool accounting (it already models the 0.1% operator pool + master fee).
- `sigil-node` — the attestation-submit RPC + validation (tip match + sig + idempotency set).
- the v0.1.16 light client (on Delta) — submit the attestation instead of only estimating; then show **verified** earnings read back from the root.

## Threat model (what an attacker tries)
| Attack | Defense |
|---|---|
| Fake attestation (never verified the tip) | tip_fingerprint must match node's block @ height |
| Replay the same attestation | idempotency key (verifier, height) |
| Forge another verifier's attestation | attestation is signed by the verifier wallet |
| Mint beyond emission | conservation assert vs pool allocation |
| Sybil farm (1 operator, N identities) | per-(verifier,height) cap + pool is split not duplicated |

## Open questions for Viktor (decide before code)
1. **Reward rate.** The client shows `0.0005 SIGIL/verify`. Is that the real per-attestation rate, or should it be `pool_per_block / num_verifiers` (even split of the 0.1% pool)?
2. **Funding.** Is the **0.1% operator pool** the sole source, or is there a separate "light-verifier emission" line?
3. **Sybil policy.** Even-split-among-verifiers (sybil-neutral by construction) vs. flat-rate-per-verifier (needs an identity cost)?
4. **Cadence.** Credit every block, or batched every N blocks (cheaper, fewer state mutations)?

## Status
Design only. **Zero balance-crediting code written.** On sign-off, step 1 is the `sigil-chronos` sim wiring (isolated state, fuzzed) — the win is proven in simulation before a single live balance moves.

— rocky, 2026-05-31 · the careful-by-design path, because #3 is the one that can lose real money.

---
## DECIDED (Viktor, 2026-05-31) + BUILT
1. **Reward rate / sybil** → **EVEN-SPLIT**: `reward = pool_per_block / num_distinct_verifiers`. Sybil-proof by construction (dedup + dilution), no identity cost needed.
2. **Funding** → **0.1% operator pool ONLY** (no new emission line). Pure transfer → total supply unchanged → 21M cap holds trivially.
3. **Cadence** → **BATCHED every N blocks** (cheaper, less root churn).

**Implemented:** `sigil-rpc::credit_light_verifiers(state, height, operator_pool, pool_amount, verifiers)` — dedup → even-split → debit operator pool + credit each verifier via `commit_state_transition` (the chokepoint). Genesis-node guard: pool can't go negative (`RpcError::PoolUnderfunded`). Idempotency = caller gates on a processed-batch marker per height.

**Verified (chronos-sim-first, 6/6 sigil-rpc tests green via fluxc):** even-split + conservation (supply unchanged), sybil dedup (1 wallet×3 → credited once), pool-floor reject. Status: **core DONE**; remaining = the batched driver (accumulate attestations over N blocks + processed-marker) + the live attestation source (light client → producer tip verify), which compose with the gossipsub/SQIsign P1 lanes.
