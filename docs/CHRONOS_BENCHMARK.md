# ⏱️ flux-chronos — Deterministic Network Benchmark

`flux-chronos` is the deterministic network simulator: one producer floods messages to N−1 sinks
under controlled latency + packet-loss, in **virtual time** (instant), seeded for **exact
reproducibility**. This is how SIGIL's gossip layer is tested without renting a single server.

**Run:** 2026-06-06 · scenario `star-flood` · 16 nodes (1 producer + 15 sinks) · 200 messages
(3000 expected deliveries) · 50 ms edge latency · seed 42.

| redundancy | drop_prob | delivered / 3000 | delivery rate | wall |
|---|---|---|---|---|
| 1 | 0.0 | 3000 | **100.0%** | 27 ms |
| 1 | 0.3 | 2094 | 69.8% | 2–4 ms |
| 3 | 0.3 | 2923 | **97.4%** | 8 ms |
| 5 | 0.3 | 2990 | **99.7%** | 11 ms |

## What it proves

1. **Deterministic.** The `drop=0.3, redundancy=1` scenario was run twice and delivered the *exact*
   same 2094 messages both times. Same `(params, seed)` ⇒ identical run — so a failure can be
   replayed and root-caused, every time.
2. **Gossip recovery.** Under a brutal **30% packet-loss**, plain flooding lands 69.8%. Re-propagating
   each message (sinks dedup by message id) climbs **69.8% → 97.4% → 99.7%** at redundancy 3 → 5.
   The network heals itself; loss is a tunable, not a failure.
3. **Instant.** 3000 deliveries across 16 nodes simulate in **2–27 ms of wall time** — virtual time
   means a whole scenario finishes faster than one real network round-trip. Thousands of these run in
   the time a real testnet boots.

> This is the "verify, don't trust" ethos applied to the network layer: prove the gossip behaves —
> deterministically, under adversarial loss — on a laptop, before it ever touches the 4-node testnet.

*Reproduce:* `flux_chronos_run {nodes:16, messages:200, drop_prob:0.3, redundancy:3, seed:42}` (the
MCP face of flux-chronos), or `fluxc`'s chronos runner directly.
