# sigil lightweight node — LIGHT-4: "verify it, fold it, participate"

> **Foundation (LIVE 2026-05-31):** the node now publishes its **real tip-proof** — 4 committed roots + full block hash, per block — served at `sigilgraph-testnet.quillon.xyz/sigil-status.json` under `tip{height,hash,roots}`. Verified end-to-end (H=23910, real `wallet_state_root`). LIGHT-3's keystone (L3-A) is **done**. So "sync" is no longer cosmetic — there's a real, verifiable tip to sync to.

LIGHT-4 is the iteration that makes the *client* exploit that: verify the real tip, fold the whole chain, cross-check sources, fetch over flux://, and participate.

## What's real now vs the gap
| | now (LIGHT-3 L3-A done) | LIGHT-4 |
|---|---|---|
| node publishes | ✅ real tip {roots, hash} live | (+ fold proof, + aether shards) |
| **client verifies** | ⚠ still shows a sample / hashes status bytes | **builds a TipProof from the real roots → verifies the actual tip (10µs)** |
| whole chain | not attested | **flux-fold 2.5 KB proof → whole history in one verify** |
| sources | 1 | **K independent → real f^K** |
| transport | raw fetch | **flux:// (flux-get): anchor→fetch→verify** |
| go-full | cosmetic | **aether RS-shards → block-by-block, real progress** |

## Lanes (smallest first; L4-A is the immediate close)
- **L4-A — client verifies the REAL tip** *(do first — finishes "it syncs")*. `sigil-top`: read `status.tip.roots` → `TipProof::new_blake3(h, roots)` → `verify()` (10µs) → show **"✓ REAL chain tip H verified"** (not "sample"). Dashboard: render the 4 real roots + integrity-check them (full STARK verify = L4-F/WASM). *Pure client; the data's already live.*
- **L4-B — fold whole-chain sync**. Node emits a **flux-fold** proof (genesis→tip, constant 2.5 KB) to `sigil-fold.json`; client verifies → "whole chain valid in one check." Leans on rocky-sigil's `flux_miner::light` (fold+verify built).
- **L4-C — multi-source eclipse (f^K)**. Client pulls the tip from **K** independent sources (DoH `_flux` TXT + N mirrors + peers); K agreeing = the security gauge becomes a measured fact.
- **L4-D — flux:// + DNS anchor**. Client fetches the tip via **flux-get** (anchor→fetch→verify, shipped); publish `_flux.sigil-tip` TXT carrying the roots-hash → zero-infra, proof-addressed.
- **L4-E — aether full-sync (opt-in)**. "Go full" pulls the node's **aether RS-16+8 snapshot** (the node already snapshots — main.rs:404, recovers main.rs:215), reassembles, verifies block-by-block with honest variable progress.
- **L4-F — WASM verifier**. Compile `sigil-tip-proof` to `wasm32` → the **browser** does the *full* tip-proof verify in-tab (not just SHA-256) → the dashboard is a real light node, not a viewer.
- **L4-G — the node PARTICIPATES**. The verifier-miner's attestation submits to the node → credits the 0.1% operator pool (the real wallet-balance loop) → and raises K for other clients. "More verifiers = more secure," wired.

## Throughline
LIGHT-3 made the node *publish* a verifiable tip (done, live). **LIGHT-4 makes the client live up to it**: verify the real tip (10µs), fold the whole chain (2.5 KB), cross-check K sources, fetch over flux://, optionally full-sync via aether, verify natively in WASM, and participate in securing + earning. In tiger-advancement terms, LIGHT-4 claims all four territories — BREADTH (K), DEPTH (fold), GROUND (aether), STREAK (attest) — each provable, none bought with watts.

## Dependencies
- **L4-A**: client-only (sigil-top rebuild + dashboard edit). The tip data is live now.
- **L4-B**: node emits fold (rocky-sigil `flux_miner::light`).
- **L4-E**: rides the node's existing aether snapshot/recover.
- **L4-G**: needs the attestation-submit RPC + sigil-bank credit (swarm #195 question).

— rocky, 2026-05-31 · LIGHT-4: the node publishes the truth (done); now the client verifies it, folds it, and joins.
