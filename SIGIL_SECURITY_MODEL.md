# Beyond "more hashrate = more secure" — the SIGIL/Flux security model

## What hashrate actually secures (and doesn't)
PoW security ∝ honest hashrate. But hashrate **only secures ORDERING** (whose chain is longest / double-spend resistance). It does **NOT** secure CORRECTNESS — a 51% attacker can reorg, but a fully-validating node *still* rejects invalid blocks regardless of the attacker's hashrate. PoW never secured validity; it just made everyone *assume* the longest chain was valid because re-validating was expensive.

## What comes after: security splits into two free + one cheap dial
SIGIL makes re-validation **free** (sigil-lite: 572 KB binary, 504 KB RAM, 743-byte proof, 10 µs verify). That collapses the old model:

| | PoW (one dial) | SIGIL/Flux (three dials) |
|---|---|---|
| **Correctness** | assumed (re-validation too costly) | **DETERMINISTIC + FREE** — any *single* honest verifier catches invalid state in 10 µs. 4 committed roots + exit-78. |
| **Ordering / finality** | longest chain (probabilistic, ∝ hashrate) | **attested-validator QUORUM** (flux-quorum + flux-nations) — final, not probabilistic |
| **Eclipse / liveness** | ∝ hashrate | ∝ **number of independent verification sources** (light clients, DNS anchors, peers) — P(eclipse) = f^K |

## The dial change
- **"more hashrate = more secure"** → **"more VERIFIERS = more secure"** — and because verification is ~free (sigil-lite), the verifier count can be *enormous* (every wallet, browser, phone). Security scales with **participation in verification**, not energy spent.
- **Cost-to-attack** shifts from **buying energy** (out-hash 51%) to **compromising a hardware-attested validator quorum** (forge M-of-N SQIsign-L5 attestations on attested hardware — flux-nations NATIONS-3). Different, and not buyable on the open market.
- **Correctness** stops being probabilistic-and-assumed and becomes **deterministic-and-verified** — the single biggest upgrade, because invalid state can *never* be made valid by any amount of work.

## The metric that replaces TH/s
`security ≈ verifier_count × attestation_diversity × (1 / verify_latency)`
— all three of which sigil-lite/flux-quorum/flux-nations make cheap, so all three can be pushed high at ~zero marginal cost. You don't burn a country's electricity; you hand every participant a 572 KB verifier.

## FLUXFOOD lanes
- SEC-1 · publish the verifier-count + attestation-diversity as live testnet metrics (sigil-lite fleet count)
- SEC-2 · NATIONS-3 hardware attestation = the new "expensive to attack" floor
- SEC-3 · eclipse-resistance: require K independent tip sources (peers + DNS anchor) before trusting a tip
