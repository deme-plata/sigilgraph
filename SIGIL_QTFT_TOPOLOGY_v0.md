# SIGIL × QTFT — the coin, topologically committed

> *We spent so much time on the physics. Here's where it pays off: the BlockDAG braid IS a knot, and SIGIL commits its topology.*
>
> Viktor directive, 2026-05-30. Drafted by rocky. Builds on DeepSeek V4's QTFT design papers (`qtft-quillon-integration.tex`, `qtft-technical-overview.tex`, `/opt/orobit/shared/QTFT/` on Beta) + project_sigil_chain lock #21 (QTFT → Flux primitive first, SIGIL inherits).

## The convergence nobody planned but everyone was circling

Three threads from this session quietly point at the same object:

1. **The Kaspa BlockDAG braid** (the never-ending left→right flow of parallel blocks with parent-edge ribbons) — Viktor's reference for 10k blocks/sec.
2. **DagKnight consensus** — parallel blocks, ordered after the fact, referencing multiple parents.
3. **QTFT** — Quantum Topological Field Theory, knot invariants for blockchain.

These are the SAME mathematical object. **A braid is the fundamental object of knot theory.** Parallel block-strands crossing and merging over time = a braid; close it up and you have a knot/link. So the DAG isn't *like* a knot — it *is* one. QTFT is simply the native mathematics of the parallel-block chain SIGIL is becoming.

## The coin update: a third kind of commitment

SIGIL's block header already commits **state** (4 roots) and **provenance** (the fluxc `.proof`). QTFT adds **topology**:

| Commitment | What it proves | Catches |
|---|---|---|
| 4 state roots | the state is what the producer claims | silent state divergence (Quillon's bug) |
| `.proof` provenance | which software produced the block | un-attested/forged builds |
| **topology commitment** (NEW) | the DAG's braid structure is what the header claims | **forks/reorgs that are topologically distinct** |

The topology commitment is a **knot invariant of the recent DAG braid**, committed in the header. Two nodes whose DAGs braid differently compute different invariants and KNOW they've forked — the topological analogue of the state-root divergence check. State divergence + topology divergence, two independent halt signals.

## The honest computational reality (this matters)

The famous knot invariant — the **Jones polynomial — is #P-hard to compute in general.** A full Jones polynomial per block over an unbounded DAG is computationally infeasible, full stop. Anyone promising "Jones polynomial consensus at 10k blocks/sec" is selling what complexity theory forbids. So:

- **Per-block topology commitment uses a CHEAP, tractable invariant** of the DAG braid over a **bounded window** (last K blocks): the **linking number** (O(crossings)) or the **Alexander polynomial** (polynomial-time via the Burau matrix determinant). These are real topological invariants, cheap to compute, and enough to fingerprint the braid's structure for fork detection.
- **Full Jones / Khovanov homology** (QTFT Path B) runs only for **deep fork adjudication** — rare, off the hot path, on a bounded sub-braid. The expensive math is the tiebreaker, not the per-block cost.
- The fluxc `.proof`'s Jones fingerprint (lock #21) is over the **MIR graph** (bounded, small) — feasible there, unlike the unbounded DAG.

This is the same honesty as the rest of SIGIL: use the cheap-but-sound version on the hot path, the expensive-but-complete version as the rare adjudicator.

## What it makes possible (the fun)

1. **Topological fork detection** (QTFT Path B / Khovanov) — complements state-root divergence. A reorg that swaps block order changes the braid's writhe/linking number → detectable as a topological event, with a proof.
2. **Braided settlement ordering** — agent-economy `SettleWork`/swap transactions are worldlines; their braiding is topologically protected (the anyonic-ordering idea from topological quantum computing). This gives the DEX a **topological fairness proof**: the sequence a block committed is the sequence its braid invariant attests — MEV-resistance you can *verify*, not just trust.
3. **The viz renders the actual consensus object.** The Kaspa-style braid in the browser viewer isn't decoration — it's the literal topological object the header commits. The knot you see is the knot that's signed.
4. **Knot-routing in p2p** (QTFT Path C) — gossip paths chosen by braid structure; a Flux-platform primitive SIGIL inherits.

## Where it lives (lock #21 work order — QTFT is a Flux primitive FIRST)

Per project_sigil_chain lock #21, QTFT lifts to Flux before SIGIL inherits:
1. `flux-consensus` — Khovanov/knot fork detection (the crate exists, 950 LOC, 77% — extend it).
2. `flux-p2p` — knot-theory routing (Path C).
3. `fluxc` — `.proof` carries the Jones fingerprint of the MIR graph (Path d).
4. THEN SIGIL inherits: the topology commitment in the header.

## Lanes (swarm board)

- QTFT-1 · `flux-topology` crate: linking-number + Alexander-polynomial of a braid (the cheap, tractable invariants) · ~2d · the tractable core
- QTFT-2 · SIGIL header topology commitment (bounded-window DAG-braid invariant) + divergence check · ~2d · needs DagKnight DAG (STAR-3/4) + QTFT-1
- QTFT-3 · deep-fork adjudication via flux-consensus Khovanov (rare path) · ~3d · research-grade
- QTFT-4 · braided-settlement topological fairness proof for the DEX/SettleWork · ~2d
- QTFT-5 · wire the braid invariant into the browser viz (the knot you see = the knot that's signed) · ~1d

## Honest limitation

QTFT-on-blockchain is genuinely research-grade — DeepSeek's papers are design, not shipped consensus. The TRACTABLE part (linking number / Alexander polynomial per block, bounded window) is real and buildable; the full Jones/Khovanov adjudication is the deep end. The economic subsystem (emission/oracle/USDS) is the higher-priority, lower-risk build; QTFT is the physics flourish that makes the coin genuinely novel once the foundations are solid. Build the floor first, then add the topology.

— rocky 🟠
