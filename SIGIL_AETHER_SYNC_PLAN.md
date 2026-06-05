# SIGIL real full-sync + flux-aether persistence — the proper build

> Viktor: "download all testnet blocks and save via flux aether. use fluxfooding and combos, do this properly." This is the plan to do exactly that — honestly, with real (variable) throughput, no faked bar.

## Why the bar is fake today (and can't be real yet)

`sigil-top` go-full currently fills a **placeholder** gauge (bounded ~5s). It cannot be a real download because **the testnet node is P2P-only — there is no block-range API**. The log-sidecar only publishes the *last ~12 blocks* (parsed from `delta.log`). To download *all* blocks we must expose the node's history. That's component 1.

## The 3 components (FLUXFOOD-composed)

```
 ┌─ Component 1: BLOCK-RANGE SOURCE (Delta) ──────────────────────────────┐
 │ sigil-node `dump-blocks --from H1 --to H2 --out <file>` — reads the     │
 │ flux-db block CF (db-delta) and writes a range as JSON/cbor.            │
 │ Published to dist-final as sigil-blocks-<from>-<to>.json (q-flux serves)│
 │ MCP combo: flux_sigil_node_dump (extends sigil_ops.rs) drives it.       │
 └────────────────────────────────────────────────────────────────────────┘
            │  q-flux static serve (already works)
            ▼
 ┌─ Component 2: CLIENT DOWNLOAD + VERIFY (sigil-top) ────────────────────┐
 │ On go-full: fetch ranges in batches (curl), verify EACH block          │
 │ (sigil-tip-proof / recompute the 4 roots) — REAL per-block verify →    │
 │ the gauge shows REAL, VARIABLE blocks/sec (it will jitter, honestly).  │
 │ Progress = blocks_verified / height.                                   │
 └────────────────────────────────────────────────────────────────────────┘
            │  each verified batch
            ▼
 ┌─ Component 3: FLUX-AETHER PERSISTENCE ────────────────────────────────┐
 │ shard_file(batch) → K-of-N erasure + blake3 encryption (flux-aether v1)│
 │ store shards to ~/.sigil/aether/ ; FileBlock manifest = local index.   │
 │ proof-of-storage (flux-aether v2) → the client can PROVE it holds them.│
 │ "full node" = chain history sharded + saved locally + verifiable.      │
 └────────────────────────────────────────────────────────────────────────┘
```

## The one combo that ties it together

`flux_sigil_sync_combo` (new, in sigil_ops.rs):
1. `flux_sigil_node_dump` — Delta exports the missing ranges to dist-final.
2. client downloads ranges, verifies every block (real rate).
3. flux-aether shards + persists each batch.
4. returns REAL stats: blocks, verified/s (variable), shards stored, total bytes, aether content_root.

## FLUXFOOD reuse (most of it already exists)

| piece | status |
|---|---|
| flux-aether v1 (shard/reassemble) | ✅ built |
| flux-aether v2 (proof-of-storage, time-lock, PIR) | ✅ built (10/10) |
| sigil-tip-proof (verify) | ✅ built |
| sigil-top dual-mode TUI (lite→full dashboard) | ✅ built |
| **sigil-node `dump-blocks` CLI** | ❌ NEW (reads flux-db block CF) |
| **flux_sigil_node_dump + flux_sigil_sync_combo** | ❌ NEW (sigil_ops.rs) |
| **sigil-top download+verify+shard loop** | ❌ NEW (+ flux-aether path-dep) |

## Why this is a fresh-context build

It spans **sigil-node** (CLI + flux-db block iteration), **Delta serving**, **sigil-top** (download loop + cross-workspace flux-aether dep), and a new **MCP combo** — multi-file, multi-host, and it must be *correct* (real verification, real persistence). Doing it in a saturated context risks exactly the kind of half-built "looks real but isn't" that the fake bar already was. It deserves a focused session — this doc makes that session start cold-clean.

## Honest interim state (shipped now)

- `sigil-top` full-mode **data is real** (height/peers/supply/block-stream from the live sidecar snapshot); the **sync gauge is a labeled placeholder** until component 1 lands.
- The fix today: the full dashboard now appears reliably (~5s, bounded) regardless of height.

— rocky, 2026-05-31

## Update 2026-05-31 — RS shipped + live-recovery TESTED (negative)
- ✅ **Reed-Solomon erasure SHIPPED** in flux-aether (`src/rs.rs` + `reed-solomon-erasure` dep). `bin/durability-proof` proves **lose 8 of 24 hosts → recover byte-identical + re-verify** (flux_combo 12/12). The can't-lose MECHANISM is real.
- ❌ **Live-node "comes back identical" TESTED + FAILED** (`scripts/recover-test.sh`): killed producer A (H=28301), restarted bootstrapped to peer B → A came back **FRESH at H=0 and produced a new chain**. P2P-sync-on-boot does NOT pull the historical chain; in-memory means restart = genesis.
- **Therefore the live demo needs a sigil-node code change** — on boot: (a) RS-reassemble the chain from aether shards (the durability-proof store), OR (b) real turbo-sync the full chain from the peer before producing. Plus the persistence/get-block foundation. This is a node-binary build, not a launch tweak.
