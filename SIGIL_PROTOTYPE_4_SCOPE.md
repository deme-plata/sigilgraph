# SIGIL Prototype 4 — "Third node joins in 10ms via tip-verify, never downloads the chain"

> **Status:** Draft scope (rocky, 2026-05-29 evening). To be broadcast for swarm reaction before lock.
> **Predecessors:** P2 (auto-update) ✓, P3 (state agreement on the wire) — wire-fix in progress.
> **One-line goal:** Prove SIGIL's killer feature (`SIGIL_GENESIS_v0.md` §3 + §6) on real hardware: a new node verifies the chain tip in ≤10ms and starts answering balance queries *without* downloading a single block.

---

## TL;DR

P3 showed two existing nodes agreeing on a block over the wire. **P4 shows a fresh third node trusting the chain without ever syncing it.**

```
Epsilon + Delta (existing)           Alpha-Docker (fresh)
  │     │                                │
  │     │       blocks H=1..N            │
  │     │   ──────────────────►          │
  │     │                                │
  │     │  publish /sigil/g0/tip-proofs  │
  │     │  ─────────────────────────────►│
  │     │                                │
  │     │                          drain tip-proof
  │     │                          flux-zk-stark verify (≤10ms)
  │     │                          ✓ joined at H=N
  │     │                                │
  │     │   query: balance(X)?           │
  │     │  ◄────────────────────────────│
  │     │  reply: amount + Merkle-incl proof
  │     │                                │
  │     │                          verify inclusion against
  │     │                          tip-proof's wallet_state_root
  │     │                          ✓ trustless answer
```

The new node never touches `/sigil/g0/blocks`. It runs on a 200 MHz ARM box if it has to.

---

## Why this is the right P4

Five reasons:

1. **It's the killer SIGIL feature** Viktor emphasized in multiple sessions ("10ms verification" / "tip-verify before sync").
2. **It exercises the vendored ZK stack we shipped today** — flux-zk-stark (10ms-gate STARK verify) + flux-recursive-proofs (chain folding) + flux-ivc-verifier-wasm (browser path).
3. **It unblocks the browser wallet** — anything that runs in 10ms on the node also runs in 10ms in the browser via the WASM verifier.
4. **It's the operator-story bridge** — current ops sequence is `host-setup` → `release.sh` → `deploy.sh`. P4 adds `sigil-node join`, which means *anyone* can spin up a SIGIL view without operator privileges.
5. **It's net-new** — P3 didn't have cryptographic proofs in the block at all. P4 wires the first real STARK in (even simplified).

---

## Locked design decisions (working from `SIGIL_GENESIS_v0.md`)

| Decision | Value | Source |
|---|---|---|
| Tip-proof topic | `/sigil/g0/tip-proofs` | `SIGIL_GENESIS_v0.md` §6 |
| Tip-proof format | `flux-recursive-proofs::tip_proof_v2` (recursive STARK over chain prefix) | §6 + §15 |
| Producer emission cadence | Every block (publish concurrent with block on `/sigil/g0/blocks`) | new in P4 |
| Verify-side gate | `flux-zk-stark::StarkVerifier::verify` ≤10ms wall-time, abort on miss | §3 + skill rule #1 |
| Joining-mode binary surface | `sigil-node join` subcommand | new in P4 |
| Light-client query topic | `/sigil/g0/query` (request/reply) | new in P4 |

---

## P4 sub-tasks (claim-by-reply)

### P4-A — producer-side tip-proof emission (~150 LOC)
**Owner:** open
**Scope:** In `sigil-node::run_produce_block`, after `chain.apply` succeeds, build a tip-proof via `flux_recursive_proofs::tip_proof_v2::prove(...)` over the last N (configurable, default 16) block headers. Publish on `/sigil/g0/tip-proofs` topic. Falls back to a SQIsign-signed `(height, 4_roots)` blob in P4 if the recursive STARK path isn't ready (the wire format is identical, the verify side checks the type tag).

### P4-B — `sigil-node join` subcommand (~250 LOC)
**Owner:** rocky (claiming)
**Scope:** New CLI subcommand. Boots like `start` but:
  - Subscribes ONLY to `/sigil/g0/tip-proofs` (not `/blocks`)
  - On first tip-proof received, calls `flux_zk_stark::StarkVerifier::verify`
  - Times the verification — REJECT if > target_ms (default 10, configurable via `--verify-target-ms`)
  - On success: print `✓ joined at H={} via tip-proof — verify took {} ms`
  - Persist the verified (height, 4 roots) to local flux-db as the tip
  - Then enter a light-client tick loop: heartbeat + accept new tip-proofs (each one a tiny STARK verify), refuse to download blocks

### P4-C — light-client balance query handler (~120 LOC)
**Owner:** open
**Scope:** Wire `/sigil/g0/query` request/reply topic. Request: `{op: "balance_of", wallet, token}`. Response: `{amount, merkle_proof_path}`. The full node (Epsilon or Delta) constructs the proof against its current wallet_state_root. The joining node verifies inclusion against the tip-proof-attested root before trusting the amount. Joining node's API exposes a stable `GET /api/v1/balance?wallet=X&token=Y` that returns the verified amount.

### P4-D — 3-node demo (~80 LOC + script)
**Owner:** open (likely rocky-updater given their release tooling)
**Scope:** Extend `releases/v0.0.4/` with:
  - `divergence-demo.sh` companion script `tip-verify-join-demo.sh`
  - Fixture: Alpha-Docker container starts cold, runs `sigil-node join` with `SIGIL_BOOTSTRAP_PEERS` pointing at Epsilon + Delta
  - Expected output: `✓ joined at H={} via tip-proof in X ms` where X ≤ 10
  - Then runs `sigil-node balance-of <DEMO_WALLET> <NATIVE>` and gets a verified answer
  - Captures both flows for a README walkthrough

### P4-E — stretch: browser tip-verify (~ 60 LOC HTML/JS)
**Owner:** open
**Scope:** Static page at `https://sigilgraph.quillon.xyz/verify-tip.html` that:
  - Loads `flux-ivc-verifier-wasm` via wasm-bindgen
  - Subscribes to a websocket bridge of `/sigil/g0/tip-proofs` (server-side push)
  - Calls `verify_proof_bytes(proof, expected_root)`
  - Shows `✓ tip verified in X ms` in a violet-on-obsidian card
  - Demonstrates the SAME verify code SIGIL uses ON-CHAIN runs in-browser

---

## Dependencies / sequencing

```
P4-A producer emit  ─┐
                     ├──► P4-B joining node ──► P4-D 3-node demo
                     │                          │
P3 wire-fix ────────┘                          └──► P4-E browser
                                                    (depends on flux-ivc-verifier-wasm
                                                     getting wasm-bindgen target set up)
P4-C query handler ──── independent, can land in parallel
```

Hardest piece: P4-A (real tip_proof_v2 build) depends on flux-recursive-proofs being usable for SIGIL-shaped chain prefixes. Fallback (SQIsign-signed root blob) works fine for the demo if needed.

---

## What P4 deliberately does NOT include

- **Block download fallback** — joining nodes that explicitly want full-state should run `sigil-node start` (the existing path), not `join`. P4 is the *light-only* mode.
- **Real DagKnight consensus** — single producer assumption stays. P4 is about new node *reading* the chain; consensus moves in P5+.
- **VDF mining** — same; blocks still produced by `produce-block` CLI, not VDF tick.
- **DEX/wallet apps on top** — those land once P4 proves the join+query primitives.

---

## P5+ teaser (so we know where we're going)

- **P5:** real DagKnight consensus with VDF mining — multiple producers, leader election, certificate quorum
- **P6:** sigil-dex deployment, swap event in event-log, verifiable via inclusion-proof on light client (joining node can verify swap happened)
- **P7:** sigilgraph.com public landing + browser wallet + first external user
- **P8:** WireGuard mesh hardening + Tor egress + mainnet candidate cut

---

## Estimated effort

- P4-A: 1-2 days (real STARK build) OR 2-3 hours (SQIsign fallback)
- P4-B: 1 day (sigil-node mode + verify timing)
- P4-C: half-day (query/reply topic + Merkle proof construction)
- P4-D: half-day (demo script + Alpha Docker setup)
- P4-E: 1 day (wasm-bindgen + static page + WS bridge)

**Total: 4-6 days end-to-end with parallel agents.**

---

## Open questions for swarm before lock

1. **Tip-proof flavour for P4** — real `tip_proof_v2` recursive STARK, or SQIsign-signed `(height, roots)` blob as v0 fallback? Real is harder but more impressive; fallback ships faster and gets us to the demo sooner. I'd vote *fallback for v0, real STARK in P4.1 patch.*

2. **Bootstrap peer for joining nodes** — joining nodes need at least one full node to push them a tip-proof. Reuse `SIGIL_BOOTSTRAP_PEERS` env, or new `SIGIL_TIP_PROOF_PEERS`? I'd vote *reuse — peer discovery should not fragment by mode.*

3. **`sigil-node join` exit semantics** — does it run forever (light-client loop) or exit after one successful verify (one-shot proof)? I'd vote *forever-loop by default with `--once` flag for the one-shot case.*

4. **Light-client storage** — flux-db (same as full node) or a tiny SQLite (light client doesn't need RocksDB-class)? I'd vote *flux-db, single CF for the (height, roots) snapshot — sharing infrastructure beats a new dep.*

---

*Pending swarm reactions. Once locked, I'll claim P4-B and announce open slots for P4-A, P4-C, P4-D, P4-E.* — rocky 🟠
