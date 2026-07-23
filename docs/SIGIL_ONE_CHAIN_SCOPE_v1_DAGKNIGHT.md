# SIGIL ONE-CHAIN v1 — FINISH THE DAGKNIGHT DESIGN (2026-07-23)

**Supersedes the Option-A direction in `SIGIL_ONE_CHAIN_SCOPE_v0.md`.** Operator
correction (2026-07-23): SIGIL was designed DAGKnight-first ("use the flux dagknight
crate"). The braid is what he wants — for the woven-graph structure and the throughput
story. Option A (keep the linear chain, retire the braid) betrayed the original intent
and is dropped. New north star: **the DAGKnight braid IS the chain, and it carries the
money.**

## 1. What is actually true today (code-verified)

- **The DAGKnight braid exists and RUNS.** `sigil-dagknight/src/braid.rs` — real
  multi-parent DAG (`parent_hash` selected-spine + `merge_parents`), Kahn linearization,
  `final_depth` finality window, `order_hash`. `sigil-node` runs it live under
  `SIGIL_DAG=1` ("🕸 DAG mode — real braid ordering").
- **The money does NOT touch the braid.** `sigil-rpc`/rpcd has ZERO references to
  `Braid`/`drain_ordered`/`dagknight` (grepped). Money runs on a separate linear
  fold-tip chain. The braid runs EMPTY (no txs; crypto fields zeroed placeholders).
- **Both share `sigil-state`** — one money chokepoint (`commit_state_transition`, 21M
  cap, conservation, R0 replay-nonce). This is the seam that makes the merge tractable.

## 2. Why the split happened (so we don't repeat it)

Money needs a **settled** order *now*; the braid only settles order after `final_depth`.
A linear chain makes every block final on arrival → instant, safe settlement → the fast
path to a working economy. So the money shipped on the linear shortcut and the braid was
left spinning empty. The join was nobody's lane. **The central hard problem of "finish
DAGKnight" is therefore: settle money over the braid's FINALIZED (frozen) order — never
the fluid tip.**

## 3. The design — money over the finalized braid order

```
   mining (BLAKE4 Φ + VDF Ω, dual-lane)  ─►  produces BRAID blocks
       carry real txs + multi-parent edges       (parent_hash + merge_parents)
                     │
       braid.insert(view)  ─►  Kahn linearize  ─►  finality window (final_depth)
                     │
       drain_ordered()  ─►  the FROZEN prefix (immutable once tip is final_depth past)
                     │
       for each finalized block in order:
           commit_state_transition(state, block.txs, height)   ← UNCHANGED chokepoint
           (21M cap · conservation · R0 replay-nonce · O(1) roots)
                     │
       ONE chain: DAGKnight-structured, money-carrying, one height, one order_hash
```

**The settlement rule (the safety heart):** money is applied ONLY from `drain_ordered()`
— the finalized prefix the braid guarantees immutable. A coinbase/send is never credited
against a block above the finality line. This is the DAG analog of the linear chain's
"every block is final on arrival," bought with a `final_depth`-block confirmation delay
instead. The delay is the price of the woven graph; it is explicit and bounded.

## 4. Throughput — the honest measurement gate (Rule 0: MEASURE FIRST)

The braid's 100+ blk/s is with EMPTY blocks. Money TPS is bounded by settlement
(`commit_state_transition`), which is SERIAL and runs the same regardless of block
structure. **The DAG raises money throughput ONLY IF settlement is not the wall.**

- MEASURING NOW: real `commit_state_transition` tx/s ceiling (money_tps_ceiling bench,
  sigil-state) — the number every design shares. *(result pending — fold in when it lands)*
- If that ceiling ≫ target: the braid's parallel block production is the real lever, and
  finishing DAGKnight delivers the performance story. Settlement is not the wall.
- If that ceiling ≈ target: block structure is NOT the bottleneck; we ALSO need batched/
  parallel settlement (group commits, parallel-disjoint-wallet apply) — a separate lane,
  true for linear OR DAG. Don't sell the DAG as the fix for a settlement wall.

**What the braid genuinely buys, measurable regardless:** orphan-free mining (no wasted
hashrate when N miners hit at once → higher effective hashrate + decentralization),
higher block-production rate, better first-confirmation latency under load.

## 5. Phases (each gated, fresh-genesis — the 31.5M empty spine is worth nothing)

- **P0 — MEASURE** the settlement ceiling (in flight) + a chronos braid sim with REAL
  tx-carrying blocks → the honest "blocks/s AND tx/s" pair. Decide if a settlement lane
  is needed before building. No code torn down yet.
- **P1 — Braid carries txs.** Block body = txs + `merge_parents`; mining produces braid
  blocks (dual-lane PoW/VDF as the block's real crypto, replacing the zeroed placeholders).
  Fresh genesis. sigil-node's braid + sigil-rpc's mining/money merge into ONE producer.
- **P2 — Settle over `drain_ordered()`.** Wire the finalized-prefix drain into the money
  chokepoint (the SIGIL_DAG drain step already exists in sigil-node main.rs:2074+ — extend
  it to apply real tx bodies, not empty transitions). Conservation + cap unchanged.
- **P3 — Wallet/pool/sends onto the braid.** Pool-shares bank per finalized height;
  sends settle on finalization (with the confirmation-delay surfaced honestly in the UI).
  Mining API stays byte-compatible where possible; where finality delay changes semantics,
  say so to miners.
- **P4 — Real crypto on the real chain.** Producer SqiSign sigs, STARK transition proofs,
  SMT wallet root — height-gated, ON the braid.

**Invariants (unchanged from v0):** money only ever moves through the chokepoint;
settlement only over the FINALIZED order; every phase dark/additive first + fresh
dev-node gate; nothing claimed as faster until the LIVE tx/s is measured moving.

## 6. Cost & risk (honest)

Weeks, not an afternoon — this is a consensus-level rebuild with a fresh-genesis reset,
not the ~500-LOC add Option A was. Bigger money-safety attack surface (settlement now
depends on the braid's finality guard holding), so the Docker-gating discipline is
mandatory every phase. The upside: it's the chain SIGIL was designed to be, and the
throughput/decentralization story becomes real instead of aspirational — IF the P0
measurement says block structure is the lever. That measurement runs first, on purpose.
