# SIGIL ONE-CHAIN — scoping document v0 (2026-07-23)

**Directive (operator, 2026-07-22):** "I don't want two hard-to-understand chains. Only one." Bitcoin
has one block number; SIGIL should too. The UI already shows only the mining chain (v7.1.4);
this document scopes making that true **under the hood**.

Evidence base: 4-agent-verified code map, all claims carry file:line (investigation 2026-07-23,
sections quoted below reference it). No numbers in this doc are guessed.

---

## 1. Current reality (measured, not remembered)

```
   CHAIN A — "DAG spine" (sigil-node, :9501)          CHAIN B — "mining ledger" (sigil-rpcd, :8099)
   ─────────────────────────────────────────          ───────────────────────────────────────────────
   height ~31.5M · ~100+ blk/s                        height ~110k · one block per accepted solve
   blocks: EMPTY (mutations=[], events=[])            blocks: DualLaneBlock (BLAKE4 PoW + Wesolowski VDF)
   crypto fields: ZEROED placeholders                 crypto: REAL — dual-lane verified, fold_tip hash chain
     (nonce, vdf_proof, producer_sig,                 balances: REAL — SigilState, signed sends,
      state_transition_proof all 0)                     strictly-increasing per-wallet nonce replay guard
   wallet_state_root: stub accumulator,               cap: REAL — MAX_SUPPLY 21M enforced in
     UNCHANGED every block, commits a                   commit_state_transition (SupplyCapExceeded
     demo-genesis state (no real money)                 rejects the whole block)
   consumers: sigil-top panels, tip-proof,            emission: time-based halving (sigil-emission),
     DNS anchor, fold sync, cathedral,                  emission_carry remainder accounting
     delivery-law probes                              consumers: wallet, explorer, miners, pool
   RUNTIME COUPLING BETWEEN THEM: NONE — separate genesis, separate state, no IPC/HTTP/gossip link.
   Shared ONLY at library level: sigil-state (SigilState, commit_state_transition, MAX_SUPPLY),
   sigil-header (SigilBlockHeaderV0, StateRoots).
```

The blunt summary: **Chain B is the ledger. Chain A is a dyno** — it proved the braid/throughput/
verification machinery, but its blocks carry no money, its proofs are placeholders, and its
committed wallet root describes a demo world. There is nothing in A's *history* worth preserving
on-chain. What IS worth preserving is A's **organs**: the header schema with four state roots,
tip-proofs, the DNS anchor, fold/succinct sync, cathedral certification — all of which operate on
`SigilBlockHeaderV0`, a type **both chains already share**.

## 2. Options

### Option A — Ledger absorbs the spine's organs (RECOMMENDED)
The mining chain becomes THE chain. rpcd wraps every accepted dual-lane block in a real
`SigilBlockHeaderV0` whose four roots come from its **real** `SigilState` (which already commits
through the same `commit_state_transition` chokepoint). All spine consumers re-point to the
ledger's headers — same type, so verify/fold/tip-proof/anchor code largely ports as-is.
sigil-node retires to a lab harness. One chain, one number (~110k), Bitcoin-shaped.
- Pros: money never moves (zero balance-migration risk); miners see NO API change; the
  whitepaper tech becomes MORE real (roots finally commit live state instead of a frozen stub);
  smallest new-code surface (~300–600 LOC in rpcd + re-points).
- Cons: the 31.5M number and the "billion-block deep sync" demo story are archived, not live;
  fold-sync now exercises a shorter chain (its correctness is unaffected).

### Option B — Spine becomes the ledger; balances migrate onto it
Mining solves become spine transactions; rpcd's state is snapshotted into the spine.
- Rejected: migrates ALL real money onto a chain whose state machinery is placeholder-grade
  (zeroed proofs, stub root, no replay guard on that path). Highest risk, longest path, and it
  preserves the one thing that has no value (empty history) at the expense of the thing that
  does (the live ledger). Violates the spirit of every balance-integrity rule we have.

### Option C — Keep both, mutually anchor (checkpoint hybrid)
Each mined block commits the spine tip hash and/or vice versa.
- Rejected as an end-state: it is literally "two chains with a rope between them" — fails the
  directive. Acceptable only as a transition trick, and Option A doesn't need it.

## 3. Recommended plan — Option A in four phases, each gated

**Invariants that hold through every phase (non-negotiable):**
- Balances: no code path may write `Node.state` outside the existing chokepoints
  (`credit_share`, `/send`→`commit_state_transition`). Header emission is READ-ONLY over state.
- Mining API (`/mining/challenge`, `/mining/submit`, pool shares): byte-compatible. Miners and
  HiveOS rigs must not notice any of this.
- Cap/emission: untouched. `SupplyCapExceeded` remains the one money gate.
- Every phase ships dark or additive first; live flips only after a fresh-state dev-rpcd
  (`:18099`, the pool-shares e2e recipe) run green + conservation checks.

**Phase 1 — Real headers on the ledger (rpcd, ~300–600 LOC, additive).**
Per accepted block: build `SigilBlockHeaderV0` { height=rpcd height, parent_hash=prev fold_tip,
mining fields from the actual DualLaneBlock (REAL nonce/vdf at last — the spine zeroed these),
four roots = `Node.state.roots()` (real, evolving), tx_count/txs_merkle_root from the block's
state transition }. Store under `header/{height:020}` beside `block/`. Serve
`/sigil-tip-live.json` + recent-headers from rpcd. Gate: headers verify with `precheck()` +
parent linkage over a fresh dev chain; roots change exactly when state changes.

**Phase 2 — Consumers re-point (mostly config + moderate block_sync adaptation).**
sigil-top block_store/chain_verify/cathedral read ledger headers; tip-proof publisher + DNS
anchor (`v=sigil1` TXT) emit the ledger tip; fold/backfill serves ledger headers over
`/sigil/backfill/1`. The explorer/wallet already read rpcd (done in v7.1.4). Gate: sigil-top
`[V]erify` walks the LEDGER genesis→tip green; DNS anchor advances with ledger height.

**Phase 3 — Spine retirement (ops, no code).**
Stop sigil-node block production; archive its DB (nothing references it at runtime — verified).
Keep the crate as the chronos/loadgen/delivery-law lab harness it factually is. The 31.5M
counter disappears from every surface (already gone from UI). Gate: 48h with zero consumer
regressions; delivery-probe re-pointed at the ledger's backfill or explicitly parked as lab-only.

**Phase 4 — Make the placeholder crypto real, on the real chain (research-grade, ongoing).**
Producer SqiSign signatures on headers; STARK state-transition proofs; upgrade the wallet root
from the additive accumulator (admits no inclusion proofs — honest limitation, documented in
sigil-state) to a real SMT so single-balance proofs exist. Each lands height-gated. This is
where the whitepaper's claims finish becoming measured instead of aspirational — on the chain
that holds the money.

## 4. Effort + blast radius (from the code map)

| Piece | Size today | Phase impact |
|---|---|---|
| sigil-rpcd daemon | 2,479 LOC | P1: +300–600 LOC additive |
| sigil-node/src | ~6,500 LOC | P3: retired to harness (no rewrite) |
| sigil-top block_sync | ~6,100 LOC | P2: re-point + wire-format adaptation (same header type) |
| sigil-top panels/verify/cathedral | part of 15.4k | P2: source-URL changes, logic unchanged |
| sigil-state / sigil-header / sigil-emission | shared libs | untouched until P4 |

## 5. Open questions (decide before P1 lands)
1. Network id of the one chain: keep `sigil-g0` (continuity of anchors/topics) vs mint `sigil-g1`
   (clean break). Lean: keep `sigil-g0` — the ledger inherits the name, topics stay.
2. `parent_hash` seam: fold_tip already chains blocks; reuse it as parent_hash (lean yes — one
   hash chain, no parallel truth).
3. Delivery-law research: keep publishing from the lab harness or re-measure on the ledger's
   (slower) block flow? Papers currently cite the harness numbers — keep host attribution honest
   per the philosophy-paper rules.
4. The TUI "supply 21,000,000/21,000,000 (100%)" panel reads the spine's display path while the
   ledger still emits time-based rewards — reconcile in P2 by reading `native_supply()` from rpcd
   (there is currently NO supply API on rpcd — add one in P1).
