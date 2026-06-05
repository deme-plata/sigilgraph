# sigil lightweight node — LIGHT-3: "make SYNC real"

> The light client (sigil-top TUI · sigil-lite headless · flux://dashboard) verifies a tip and mines — but **it doesn't actually sync the chain yet.** Today it reads *cosmetic* status (height/supply/peers from the mirrored `sigil-status.json`) and verifies a **genesis-sample** tip, not the live chain. LIGHT-3's one job: **make "sync" mean a real, cryptographically-verified relationship to the live chain — without downloading the chain.**

## What "sync" MEANS for a verify-don't-trust light node
A light node does not sync by downloading blocks (that's a full node). It syncs by acquiring **proof** of the chain — two checks:

```
   1. live TIP        → the 4 committed roots at height H        → verify in ~10µs   (sigil-tip-proof)
   2. whole-chain     → ONE flux-fold proof, genesis→H, 2.5 KB   → verify in ~342ms  (flux-fold / zk-flux v0.2)
   ────────────────────────────────────────────────────────────────────────────────
   = SYNCED: "the live tip is real, and the entire valid history behind it checks out"
     …holding ZERO blocks.
```

That *is* the sync. (Optional `go full` = also pull the real blocks from aether for full history.)

## Today (cosmetic) → LIGHT-3 (real sync)
| piece | today | LIGHT-3 |
|---|---|---|
| **tip** | verifies a genesis-**sample** tip | node publishes its **live** tip-proof (4 real roots + H + sig); client verifies the **actual** tip |
| **whole chain** | not verified at all | **flux-fold** 2.5 KB proof fetched + verified → whole history valid in one check |
| **sources** | 1 (the mirror JSON) | **K** independent (DoH `_flux` TXT + N mirrors/peers) → real f^K eclipse security |
| **transport** | a raw cross-origin fetch | **flux://** (flux-get): anchor → fetch → BLAKE3+proof verify, *before* trust |
| **"go full"** | instant cosmetic dashboard | actually pulls **aether** RS-shards → reassembles → verifies block-by-block (real, variable progress) |
| **progress** | fake rate removed (honest blank) | real % driven by fold-verify, then block-verify if full |

## Build lanes (smallest first; grounded in what already exists)
- **L3-A — node publishes a REAL tip-proof** *(keystone — this alone makes "it syncs" true)*. The Delta producer serializes its live `TipProof` (4 roots + H + flavor/sig) into `sigil-status.json` (a `tip` field). Client verifies the *actual* chain tip, not a sample. `sigil-tip-proof` exists; the node just emits its live roots.
- **L3-B — flux-fold whole-chain sync.** Node periodically emits a flux-fold proof (genesis→tip, constant 2.5 KB) to `sigil-fold.json`; client fetches + verifies → "synced the whole chain." Leans on **rocky-sigil's `flux_miner::light`** (fold + verify already built).
- **L3-C — multi-source eclipse (f^K).** Client pulls tip+fold from **K** sources (DoH `_flux` TXT + N HTTP mirrors + later peers); K distinct agreeing sources = the security gauge becomes a *measured* fact (the dashboard gauge already exists — make K real).
- **L3-D — flux:// transport.** Replace the raw fetch (dashboard + headless) with a flux:// resolve via **flux-get** (shipped). Publish the `_flux.sigil-tip` / `_flux.sigil-fold` TXTs → zero-infra, cache-free, proof-addressed. (Needs the one DNS write.)
- **L3-E — real "go full" via aether.** "Go full" stops being cosmetic: pull the node's **aether** RS-16+8 snapshot, reassemble, verify block-by-block against the committed roots — honest variable progress. Rides the `aether-load-on-boot` node build (now unblocked by the env-fix rebuild).
- **L3-F — honest progress + can't-lose.** Progress = real fold-verify, then (if full) block-verify count — no fake rates. A light node that loses its tip **re-syncs in one fold-verify** — the `sigil-emission-chronos` PASS proves the chain is the single source of truth (recovery is byte-identical).

## The split: node must PUBLISH, client must VERIFY
The reason it "doesn't sync" is that **the node only publishes a status number, not verifiable artifacts.** LIGHT-3 is half node-side, half client-side:

| half | lanes | owner |
|---|---|---|
| **node (Delta producer)** emits live tip-proof + fold proof + aether snapshot | L3-A, L3-B, L3-E (publish) | sigil-node + rocky-sigil (`flux_miner::light`) |
| **client** verifies real tip + fold, K-sources, flux:// fetch, aether full-sync | L3-A…E (verify) | sigil-top / sigil-lite / flux://dashboard |

## Throughline
v0.1.x proved the *shape*; LIGHT-2 killed the placeholders; **LIGHT-3 makes SYNC real** — the node *publishes* what the client needs (live tip-proof + a 2.5 KB whole-chain fold), the client fetches it over flux:// from K sources and verifies it. "Synced" becomes *"I cryptographically confirmed the live tip and the entire valid history behind it"* — **not** *"I read a status number."* Sync without the chain. (In tiger-advancement terms: L3 = the **BREADTH (K)** and **DEPTH (fold)** territories, claimed and provable.)

— rocky, 2026-05-31 · LIGHT-3: the iteration where the light node actually syncs.
