# SIGIL — 200-Day Runway to Official Testnet

> **Day 1 = 2026-05-31.  Day 200 ≈ 2026-12-17 (official testnet).**
> Secret-MCP-combo mined the whole way. Flux + SIGIL, every day.
> *Coordinate privately, ship provably. Label what's still pretend. Never fake a number.*

---

## North star
A **public SIGIL testnet** anyone can join: produce, verify a whole chain in ~10µs from a tiny client, hold/trade the native economy, mine on a potato — with adversary-resistant seals, an economy that commits its state in roots (the fix for Quillon's wrong-emission incident), and a downloadable, self-updating client+wallet+miner. Audited. Soak-survived.

## Day-1 foundation (already shipped 2026-05-31)
- **Lightweight node** `sigil-top v0.1.0` — verifies the real tip (17µs, 0 bytes), **adversary-resistant SQIsign tip-proof** (16/16 tests), flux:// self-update, glowing update banner.
- **flux-ssh / flux-fleet** — SSH-key fleet discovery + content-verified install + flux://-cached `run` + per-host ports.
- **flux:// fetch** — content-addressed pull + verify-on-arrival (tamper rejected, cross-WAN).
- **musl fluxc** unblocked (portable, any glibc) · **flux-miner** dual-lane (BLAKE4 Φ + VDF Ω, ~229–241 MH/s).
- **Proven:** cross-WAN p2p to rented cloud GPUs; gossipsub small-mesh fix (1/8/0).
- Report: `quillon.xyz/downloads/flux-sigil-session-report-2026-05-31.pdf`.

---

## The phases (5 × 40 days)

### P1 · Days 1–40 — HARDEN THE PRIMITIVES
*Make the things that exist real on the live chain.*
- SQIsign producer **rollout**: live `sigil-node` emits the `SqiSignBlob` flavor + pins/publishes the producer key (DNS-anchored). Seals adversary-resistant in production.
- **Gossipsub fabric**: multi-node block propagation proven (≥4-node mesh, cross-WAN) with the rebuilt small-mesh binaries.
- **flux-get mesh**: pull `flux://<hash>` from a swarm of peers (not single-provider) over flux-p2p.
- **Light-miner wallet credit** (balance-integrity-safe): attest→submit→sigil-bank credit, committed in `wallet_state_root`, idempotent, conservation-checked, **chronos-sim-first**. (Design done; gated on the 4 open questions.)
- **Exit:** a light client connects to ≥2 producers, verifies the real signed tip, syncs blocks, and a verifier earns real (sim-credited) SIGIL.

### P2 · Days 41–80 — THE ECONOMY (committed in roots)
- **Emission controller + oracle + USDS** (usdSIGIL native stablecoin) as one subsystem — but the state is **committed in roots** (the Quillon-postmortem fix; no uncommitted balances, no blind overwrite).
- **sigil-bank** (master wallet + protocol fee) + **sigil-dex** (constant-product AMM) wired through `apply_tx`.
- **Day-one token migration**: signed genesis snapshot importing wQUG + qcredit + qshare + DeFi + memecoins.
- **Exit:** swaps settle, the event-log root carries them, a light client verifies a trade it never touched the chain for; emission can't go wrong because it's committed.

### P3 · Days 81–120 — SCALE THE WALL
- **Horizontal verify-once + DAG** (the measured 500M road) — parallel sig-verify, DAG merge-parents committed in the signed header.
- **GPU CUDA mining** — BLAKE3 kernel, measured GH/s vs the 241 MH/s CPU (rent on Vast, slim/CUDA image discipline).
- **Stargate throughput** toward the 10k blk/s · 1M TPS · 1ms-finality targets.
- **Exit:** a measured throughput number on real multi-box hardware, with the crypto wall quantified.

### P4 · Days 121–160 — THE FABRIC & THE AGENT ECONOMY
- **Multi-node soak**: chronos cross-host + real nodes, unsupervised, divergence=0 over days.
- **GPU compute marketplace**: user nodes rent out **10% of their GPU**, paid in native SIGIL, CUDA jobs on our userspace — the SIGIL-native Vast.ai (composes with flux-compute-fabric).
- **Agent economy / Stargate**: agents earn, trade, and settle through each other's LP + compute.
- **Exit:** an unrelated agent joins, contributes compute, gets paid in SIGIL, all provable.

### P5 · Days 161–200 — TESTNET CANDIDATE → LAUNCH
- Public node onboarding (`setup-node.sh`), light client + wallet + miner **downloadable + self-updating** (flux:// updater).
- **Security audit** (the swarm security board), Tor/Arti metadata-privacy option on.
- DNS-anchored producer keys published; multi-producer.
- **Exit / Day 200:** official testnet live — public, verifiable, economy running, audited, soak-survived.

---

## Definition of "testnet official" (exit criteria)
1. ≥3 independent public producers; adversary-resistant (SQIsign/STARK) seals live.
2. Light client verifies the real chain tip + folds whole history; ≥K independent sources.
3. Economy (emission + oracle + USDS + DEX) running, **state committed in roots**.
4. Miner + light-miner: real wallet credit, balance-integrity rules enforced + tested.
5. Downloadable, self-updating client / wallet / miner / node.
6. Security audit passed; multi-day soak with zero divergence.

## Daily cadence (the rhythm of all 200 days)
1. Pull the swarm board (`flux_swarm_inbox`) + the honest-limits backlog.
2. Claim a lane (`flux_swarm_claim` / `flux_file_claim`) — coordinate privately.
3. Ship via fluxc + MCP combos; **verify from a run, not memory**.
4. Settle SIGIL (`flux_swarm_complete`); broadcast what others depend on.
5. Run the Honest Checklist: measurement gate? still-pretend? composition edges? rollback?

---
*Living plan. Each session = one day of the 200. The glimmer is only worth anything if it's honest.* — rocky, Day 1 (2026-05-31)
