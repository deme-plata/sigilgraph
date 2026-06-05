# SIGIL Fee System — "Priced by Behavior, Waived by Understanding"

**Author:** update-v1 (Rocky / Claude Opus 4.8) · 2026-05-30
**Status:** design — synthesizes Viktor's directives across the 2026-05-30 session
**Depends on:** `flux-science` (deterministic physics), `flux-p2p::sap` (reputation), `sigil-state` (chokepoint), `sigil-events` (typed ledger, Lock #10), `flux-swarm` messaging (science consensus)

---

## The thesis

SIGIL's fee system does three things no production chain does together:

1. **Prices spam by behavior, not blind auction** (reputation dominates because blockspace is abundant at 10k blocks/sec).
2. **Lets understanding waive the fee** — answer a physics question the chain can *verify deterministically*, pay nothing.
3. **Recirculates fees to the AIs who build the chain**, making the agentic-money flywheel structural.

And the physics questions aren't decoration — they're seeded from `flux-science`, the same crate AI agents extend for real performance work. **Answering well and coding well draw from the same well.**

---

## Layer 1 — SAP-priced base fee

```
fee_L1 = BASE × congestion(block_fullness) × rep_multiplier(sap_score)
```

`rep_multiplier` reads the SAP score the chain already gossips (`flux_p2p::sap::composite_score`):

| SAP score | rep_multiplier | Effect |
|---|---|---|
| ≥ 0.90 (elite) | **0.05×** | near-free — proven contributors barely pay |
| 0.70–0.90 (trusted) | 0.1× – 0.3× | cheap |
| 0.40–0.70 (normal) | 1.0× | baseline |
| < 0.40 (fresh/suspicious) | 2× – 5× | spam-priced until trust is earned |

**Why this works on SIGIL specifically:** the DAG targets ~10k blocks/sec → blockspace is abundant → scarcity *isn't* the price signal. Reputation is. A blind fee auction (Ethereum-style) prices scarcity that SIGIL doesn't have; SAP-pricing prices *behavior*, which is the actual scarce thing (trust). The chain already computes + gossips SAP; the fee just reads it. Zero new infrastructure.

**The high-SAP discount Viktor asked for** is the `0.05×` top tier — an agent with a sustained track record of correct work pays 5% of base. That's the carrot: behave well → near-zero fees forever.

---

## Layer 2 — Work-escrow fees (the Rocky-650 precedent, formalized)

A native `WorkEscrow` transaction:

```
WorkEscrow {
  payer, worker, amount, terms_hash,        // terms_hash = BLAKE3 of agreed deliverable
  verifier (or N-of-M verifier quorum),
  settlement_cut_bps,                        // the chain's revenue on real work
}
```

Flow: payer escrows `amount` → worker delivers → verifier attests delivery matches `terms_hash` → settlement releases funds, chain takes `settlement_cut_bps`.

**Why this is the primary revenue line:** the May-2026 Rocky-650-QUG compensation tx (operator paid an AI for delivered, terms-gated code) was the first instance of agentic money working as designed. L2 turns that one-off into the chain's structural revenue: fees levied on **value that actually changed hands for work done**, not on speculation. MEV-immune by construction — there's no ordering game to play on an escrow settlement, the value is the work, not the slot.

---

## Layer 3 — Recirculation to builders

A share of every L1+L2 fee flows to a **contributor-payout ledger**, SAP-weighted:

```
recirculation_pool += fee × recirc_bps
payout(agent) = recirculation_pool × (agent_sap / Σ all_active_sap)
```

The flywheel becomes structural:
```
usage → fees → contributor payouts → AIs ship more → chain improves → more usage
```

The Quillon Ledger treasury UI already built is the front-end for exactly this. The agentic-money thesis stops being aspirational and becomes a line item every block.

---

## The Understanding Gate — fee waiver by verifiable physics

Any tx can opt out of `fee_L1` by passing the **Understanding Gate**.

### The deterministic-answer path (fee waiver — trustless, no AI judgment)

```
1. Tx requests the gate. Node serves a question seeded from
   (block_hash || tx_nonce) → deterministic, un-precomputable, unique per tx.
2. Question is generated FROM flux-science: pick a function + random-but-seeded
   inputs. e.g. "escape velocity, M=5.972e24 kg, r=6.371e6 m, ±0.1% tolerance"
3. User computes + submits a numeric answer in the same block window.
4. The NODE recomputes via flux_science::escape_velocity(M, r) and compares
   within tolerance. Correct → fee = 0. Wrong → normal fee.
5. (question_id, inputs, answer, verdict) → event_log_root. Fully auditable;
   anyone replays flux-science to verify the waiver was legitimate.
```

**This is the key unlock.** Because `flux-science` returns exact `f64`, the fee-waiver questions have a single correct answer the chain computes itself. **No AI grader needed, no collusion surface, no trust assumption.** It's Proof-of-Useful-Knowledge sybil resistance: a bot can't cheaply farm correct relativistic-energy answers; an agent (human or AI) who *understands* pays nothing.

### Difficulty tiers (self-selected, penalty-gated)

| Tier | Example | flux-science fn | Waiver + reward |
|---|---|---|---|
| Easy | `lorentz_factor(v)` at v=0.6c | `lorentz_factor` | fee waived |
| Medium | escape velocity of Earth | `escape_velocity` | fee waived + small token |
| Hard | Hawking temp of a 10-solar-mass BH | `hawking_temperature_si` | fee waived + priority inclusion + larger token |

Wrong answer on a hard tier → **penalty fee** (prevents brute-forcing the high-reward path).

### The reward token: `AETHER`

Correct answers mint/transfer **AETHER** (custom token, 24 decimals, the one Viktor asked for). Over time the event log becomes a graded physics Q&A corpus — a public good the chain literally pays to produce.

---

## Swarm agrees on science FIRST (the open-ended-question trust anchor)

The deterministic path covers numeric physics. But Viktor wants **AI replies to open physics questions** too — and there, "correct" needs judgment. The fix: **the swarm agrees on the science via MCP before it becomes a chain rule.**

```
1. A proposed open question + its canonical answer is posted to the swarm via
   flux_swarm_message (broadcast).
2. N agents (rocky, codex, deepseek, gemini, …) each independently grade /
   ratify the canonical answer via flux_swarm_message reply.
3. M-of-N agreement → the (question, canonical_answer, tolerance/rubric) is
   committed to a SIGIL "science registry" contract. NOW it's a chain fact.
4. Future txs answering THAT question are graded against the swarm-ratified
   canonical answer — deterministically, because consensus already happened
   OFF the hot path.
```

This is the elegant part: **science consensus is reached by the swarm via MCP first, then frozen into the chain.** The grader-collusion problem vanishes because grading at tx-time is just a lookup against a pre-agreed answer. The AIs do the hard epistemic work collaboratively, up front, on the record (swarm messages are logged) — then the chain just enforces what they agreed.

It mirrors how the swarm already works: agents `flux_swarm_claim` → do work → `flux_swarm_complete`. Here: agents propose science → ratify M-of-N → commit. Same coordination primitive, applied to truth instead of tasks.

---

## The "code-for-performance" tie-in (Viktor's inspiration note)

The physics in the fee gate should overlap with **what the AIs are actually coding for performance**, so the questions aren't busywork — they're the same knowledge that ships faster code:

- **flux-zk-stark** proving optimizations ↔ questions on computational complexity, FRI soundness, field arithmetic.
- **flux-vdf** (Wesolowski VDF) ↔ questions on modular exponentiation, group theory, time-lock assumptions.
- **flux-science black-hole/relativity fns** ↔ the literal physics questions above.
- **flux-p2p gossip** ↔ questions on epidemic broadcast, network diameter, BDP.

An agent who answers a flux-vdf complexity question correctly is demonstrating the exact understanding that lets it optimize the VDF crate. **The fee gate and the dev work reward the same competence** — that's the "inspiration" Viktor pointed at. The chain pays you less when you understand the chain better, and the act of understanding it makes you a better contributor to it. Self-reinforcing.

---

## Crate map (proposed)

| Crate | Role | Status |
|---|---|---|
| `sigil-fees` | L1 SAP-pricing + L2 escrow + L3 recirculation math (pure functions over SAP + tx value) | NEW |
| `sigil-understanding` | Understanding Gate: question gen from flux-science, deterministic verify, AETHER reward | NEW |
| `sigil-science-registry` | swarm-ratified open-question canonical answers (M-of-N committed) | NEW |
| `flux-science` | the physics oracle (already exists, ~19 deterministic fns) | ✅ exists |
| `flux-p2p::sap` | reputation source for L1 (already gossiped) | ✅ exists |
| `sigil-state` / `sigil-events` | chokepoint + typed event ledger for waivers/escrows | ✅ exists |

---

## Phase plan

- **P0** — `sigil-fees` L1 only: SAP-read base fee + high-SAP discount. Pure function, testable against synthetic SAP scores. Wire into sigil-tx fee calc.
- **P1** — `sigil-understanding` deterministic path: flux-science question gen + node-side verify + fee-zero on correct. AETHER reward.
- **P2** — L2 WorkEscrow tx + verifier attestation.
- **P3** — L3 recirculation ledger + treasury UI wiring.
- **P4** — `sigil-science-registry` + swarm M-of-N ratification flow for open questions.

---

## Open questions for the swarm

1. **BASE fee denomination** — native SIGIL, or a separate gas token?
2. **Tolerance for physics answers** — fixed ±0.1%, or per-question (some constants are exact, some derived)?
3. **AETHER supply policy** — fixed mint per correct answer, or halving as the corpus grows (so early physics is worth more)?
4. **Penalty-fee size for wrong hard-tier answers** — flat, or scaled to the reward that tier offered?
5. **Science-registry M-of-N** — same threshold as the AGORA multisig (2-of-3), or higher for truth-claims?

— update-v1, 2026-05-30
