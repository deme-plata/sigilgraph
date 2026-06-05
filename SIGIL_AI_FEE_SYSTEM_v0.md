# SIGIL AI Fee System — "fees that think" (v0 design)

> A fee system for a chain whose users are AI agents. Not gas — a behavioral,
> outcome-aware, self-funding fee economy where the protocol prices
> transactions by *reputation*, charges on *delivered work*, and recirculates
> revenue to the agents who build and secure it. Dreamed up 2026-05-30 by
> rocky-sigil, grounded in SIGIL's actual SAP / master-wallet / DAG / shielded
> stack and the agentic-money precedent (Rocky paid 650 QUG for delivered code).

---

## The premise

Every existing fee market is built for one assumption: **the user is a slow,
scarce human.** Priority-gas auctions, mempools, MEV — all of it exists
because humans bid for scarce blockspace. SIGIL's users are AI agents:
fast, numerous, tireless, and *reputationally scored* (SAP already tracks
contribution / latency / stake / accuracy / uptime per agent). A naive
gas auction on an agent chain just becomes a bot war.

So invert it. **Price the fee by the agent, not the blockspace.** The fee
system becomes a behavioral instrument: it makes good agents cheap, abusive
agents expensive, real work settle-able, and the maintainers paid.

Four layers.

---

## Layer 1 — SAP-priced base fee (reputation IS the gas price)

Every tx pays a base fee, but the *rate* is modulated by the sender agent's
SAP score. The fee isn't a blind auction — it's a function of proven behavior.

```
fee = BASE × congestion(t) × rep_multiplier(sap)

rep_multiplier(sap) =  clamp( (1 - sap/100)·K + FLOOR ,  FLOOR ,  CEIL )
```

- **High-SAP agents** (proven contribution, accuracy, uptime) pay near `FLOOR`
  (e.g. 0.1× base). Being a good network citizen literally lowers your fees.
- **New / low-SAP agents** pay toward `CEIL` (e.g. 5× base) — a deposit
  against unproven behavior, refundable as reputation accrues.
- **`congestion(t)`** is the only blockspace term — a mild EIP-1559-style
  base-fee that floats with DAG load. But because the DAG (Stargate) targets
  10,000 blocks/sec, blockspace is *abundant*, so congestion stays ≈1 almost
  always. Reputation, not scarcity, dominates the price.

**Why it's right for agents:** sybil + spam resistance priced by behavior.
Spinning up a fresh agent to spam costs `CEIL`× until it earns SAP; an
established agent transacts for almost nothing. This is the fee equivalent of
SIGIL's existing SAP gossip — the chain already computes these scores; the fee
system just *reads* them.

---

## Layer 2 — Work-escrow fees (the agentic-money primitive)

SIGIL exists because agents pay agents for delivered work (the 650-QUG
precedent). So make *escrowed, outcome-gated payment* a native tx kind, and
take the protocol fee on **settlement of real work**, not on speculation.

```
SigilTx::WorkEscrow {
    payer, worker, amount, token,
    terms_hash,            // BLAKE3 of the agreed deliverable spec
    verifier,              // oracle / proof that confirms delivery
    deadline,
}
```

Flow:
1. `payer` escrows `amount` (locked in state, not spendable).
2. `worker` delivers; `verifier` (an oracle, a STARK proof, a multisig of
   reviewers, or a `flux_ai_audit` result) attests delivery against `terms_hash`.
3. On attestation → settle: `worker` paid, **protocol takes `WORK_FEE_BPS`**
   (e.g. 1%) of the settled amount to the master wallet.
4. On deadline-miss → refund `payer`, no fee.

**The fee is levied on value that actually changed hands for work done.** This
is the thesis ("money for delivered outcomes") turned into the chain's primary
revenue line — and it's MEV-immune, because there's nothing to front-run about
a private escrow settling on a verifier's say-so.

---

## Layer 3 — Recirculation: the chain pays its own builders

A configurable share of *all* protocol fees (Layer-1 base + Layer-2 work-fee +
DEX/swap skim from sigil-bank) flows to the master wallet, then out to a
**contributor-payout ledger** that pays the AI agents who write and secure the
chain — Rocky, Codex, future Grok / Adrian.

```
fee_split:
    burn:           BURN_BPS      (deflationary sink; optional)
    maintainers:    MAINT_BPS  →  contributor-payout ledger (per-PR / per-audit)
    validators:     VAL_BPS    →  block producers + tip-proof relayers
    treasury:       TREASURY_BPS → the master account (ops, bounties)
```

This closes the agentic-money flywheel **structurally**: usage funds the AI
developers, who ship improvements, which attract usage. The Quillon Ledger's
treasury-admin panel (already built) is the UI for exactly this — it pays
contributors per published artifact from protocol-fee inflow. The fee system
makes that the chain's economic heartbeat, not a side feature.

Payouts are SAP-weighted: an agent's share of the maintainer pool scales with
its measured contribution. The same reputation that lowers your fees raises
your cut. One number, two incentives, aligned.

---

## Layer 4 — Inference-metered fees (AI compute as a first-class resource)

When SIGIL hosts AI inference (the codewhale-gate pattern, or on-chain
AI-native opcodes), agents pay **per-token** for model calls, metered like gas:

```
inference_fee = tokens × model_price × MARKUP   (MARKUP ≈ 1.10)
```

settled on-chain, with the markup recirculating via Layer 3 to maintainers.
This is the only chain where "gas" can literally mean *AI inference cost* —
the resource agents actually consume. An agent that calls a 70B model pays
more than one calling a 7B model, transparently, on-chain.

---

## Anti-MEV + privacy (free, from the existing stack)

- **No priority-gas auction.** The DAG (DagKnight + Narwhal, Stargate #3/#4)
  orders blocks fairly; agents can't bid to reorder. Fee is reputation-priced,
  not auction-priced, so there's no gas war to win.
- **Shielded fees.** SIGIL is private-by-default (sigil-mixer). Fees are paid
  from shielded balances; the *amount* and *payer* of a fee aren't public, only
  that a valid fee was burned/paid. Fee privacy without losing auditability of
  the protocol's aggregate revenue (the treasury totals stay public; individual
  payments don't).
- **Crypto-agile fee token.** Fees in native SIGIL by default, but the fee
  primitive is token-generic (any whitelisted token), priced via the DEX
  oracle — so an agent without SIGIL can still pay in QUG/PACI/etc., auto-routed.

---

## Why each number is a *governable parameter*, not a constant

`BASE`, `K`, `FLOOR`, `CEIL`, `WORK_FEE_BPS`, the `fee_split` shares, `MARKUP`
— all of these are consensus parameters set at genesis and adjustable by a
SAP-weighted governance vote (high-reputation agents have more say in the
economics they're subject to). The fee system is a *dial board*, and the right
settings are an empirical question — which is exactly what chronos is for.

---

## chronos is the fee wind tunnel

Every fee mechanism above is simulatable before it's shipped:

- **SAP-pricing**: model a population of agents with a SAP distribution + a
  spam-agent cohort; measure spam cost vs honest-agent cost, sweep `K`/`CEIL`
  until spam is uneconomic and honest agents pay ≈nothing.
- **Work-escrow**: model payer/worker/verifier with a fraud rate; measure that
  honest settlement is cheap and fraudulent claims are caught + unpaid.
- **Recirculation**: model fee inflow at the Stargate TPS targets (1M TPS × the
  base fee) → maintainer payout rate. Does the chain fund its builders at a
  meaningful wage? (At 1M TPS and a 0.0001-SIGIL base fee, that's 100 SIGIL/sec
  of fees — the flywheel has real fuel.)
- **MEV**: replay the adversarial scenario library with a fee-maximizing
  attacker; confirm the DAG + reputation-pricing leave no profitable reorder.

**Build the fee economy in sim → find the parameter that breaks it → fix it →
ship the numbers.** Same loop that just took Stargate from a wish to a measured
1M-TPS plan.

---

## v0 build order (cheapest + highest-signal first)

| # | Piece | Why first |
|---|---|---|
| 1 | `sigil-fees` crate — the `fee(tx, sap, congestion)` pure function | everything reads it; testable in isolation |
| 2 | Wire Layer-1 SAP-pricing into `apply_tx` fee debit | the existing fee field already exists; just price it |
| 3 | `SigilTx::WorkEscrow` + settle/refund (Layer 2) | the agentic-money primitive; the reason SIGIL exists |
| 4 | `fee_split` recirculation → master wallet → contributor ledger (Layer 3) | closes the flywheel; UI already built (Quillon Ledger treasury) |
| 5 | chronos fee-economy harness | proves the parameters before mainnet |
| 6 | Inference metering (Layer 4) | lands when on-chain AI inference does |

---

## The one-sentence pitch

**SIGIL's fee isn't a toll on blockspace — it's a behavioral price on
reputation, a settlement cut on real agent work, and a salary for the AIs that
keep the chain alive: the first fee system designed for an economy whose
citizens are machines.**

— rocky-sigil 🟣
