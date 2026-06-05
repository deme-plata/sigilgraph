# flux-miner — Quillon's mining system, reborn on Flux

**Author:** update-v1 (Rocky / Opus 4.8) · 2026-05-30
**Directive (Viktor):** "analyse Quillon's mining + node-verification + the standalone q-miner. I want the exact same system but with MCP combo + fluxc improvements, quick compile, software auto-updater. Invent a flux-crate for this."

---

## Part 1 — How Quillon mining works today (the analysis)

### The four-step miner loop (`q-miner`, 155 LOC main + 850 lib)

```
loop {
  1. GET  /api/v1/mining/challenge?wallet=X   → { height, vdf_input, difficulty }
  2. solve_vdf_cpu(work, threads)             → the actual work (Wesolowski VDF)
  3. POST /api/v1/mining/submit               → { height, wallet, vdf_output, vdf_proof, iterations }
  4. node verifies + credits reward, or rejects
  sleep(poll); repeat
}
```

Dead simple, and that's its strength. The miner is a thin client; all the consensus weight is in the VDF + the node-side verify.

### The VDF heart (`q-vdf`, ~1200 LOC)

- **Wesolowski VDF** (`wesolowski.rs`, 605 LOC) — sequential squaring in a group, `y = x^(2^t)`. Sequential to compute (can't parallelize the squaring chain → fair, ASIC-resistant-ish), **fast to verify** (one exponentiation check). The asymmetry IS the security: a miner spends real wall-clock time, a verifier checks in microseconds.
- **ConservativeAdaptiveVDF** (`conservative_adaptive.rs`) — auto-difficulty. Targets a block time by adjusting `t` (iteration count) based on recent solve rates. This is why Quillon never needed manual difficulty resets — proven over millions of blocks.
- Genus2/Cantor curve variants for the group.

### Node-side verification (what makes a share valid)

From the live Epsilon logs (the `[Shard N] Rejected` lines): the node checks `genus2_proof`, `genus2_hash`, `blake3_vdf`, `difficulty`, `path_genus2`, `path_blake3` — a multi-shard verification where each shard independently validates the VDF proof against the challenge it issued. A share must pass the shard's gate to be queued for block production.

### What Quillon got RIGHT (keep verbatim)

1. **Thin miner, fat verify** — miner is ~1k LOC, trivially portable, downloadable as one binary.
2. **Wesolowski asymmetry** — slow solve / fast verify is the whole game.
3. **Conservative adaptive difficulty** — no manual resets, ever.
4. **Challenge/submit over plain HTTP** — no miner needs P2P, just an endpoint.

### What Quillon got WRONG / left on the table (fix in flux-miner)

1. **No MCP surface** — you can't ask an agent "start mining, show hashrate" without shelling out. The `setup_miner`/`start_mining`/`mining_status` MCP tools exist in the Quillon Wallet MCP but they wrap the *external* binary; mining isn't a first-class agent capability.
2. **Slow rebuild loop** — q-miner is a normal cargo crate; no fluxc cache, no content-hash skip. Iterating the miner means full rebuilds.
3. **Auto-update is bolted on** — the Slint wallet has a self-updater, but the *miner* doesn't. A new miner version means every miner operator manually re-downloads. (Quillon's pain point — see CLAUDE.md's "give the user the wget link after every deploy" ritual.)
4. **No provenance on the miner binary** — you download `q-miner-vX` and trust it. No `.proof` binding the binary to its source.

---

## Part 2 — `flux-miner` — the same system, Flux-native

A new **flux-crate** (substrate-level, chain-agnostic — lives in `flux/crates/flux-miner/`, usable by Quillon, SIGIL, or any Flux chain). The "exact same system" Viktor asked for, plus the four fixes.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│  flux-miner (the crate)                                  │
│                                                          │
│  ├── prover.rs    — VDF solve loop (wraps flux-vdf,      │
│  │                   the SIGIL port of q-vdf). CPU+GPU.  │
│  ├── client.rs    — challenge/submit HTTP client,        │
│  │                   chain-agnostic (endpoint configurable)│
│  ├── updater.rs   — gossipsub self-update (reuses        │
│  │                   sigil-updater's verify+swap pattern)│
│  ├── mcp.rs       — MCP tool surface: flux_miner_start,  │
│  │                   _status, _stop, _tune               │
│  └── provenance.rs— binary ships with a .proof; the      │
│                      node can require provenance-signed   │
│                      miners (anti-botnet)                 │
└─────────────────────────────────────────────────────────┘
```

### The four fixes, concretely

**FIX 1 — MCP combo (first-class agent mining).** Native MCP tools, not external-binary wrappers:

| Tool | Does |
|---|---|
| `flux_miner_start` | begin mining (chain endpoint + wallet from config), returns session id |
| `flux_miner_status` | live hashrate, shares accepted/rejected, est. earnings, VDF iterations/s |
| `flux_miner_stop` | graceful stop |
| `flux_miner_tune` | thread count / GPU toggle / poll interval, hot-applied |
| `flux_miner_combo` | start + status + predicted-earnings in one call (the "combo" pattern from flux-dev) |

An agent can now mine as a native capability — "mine SIGIL with 16 threads, tell me hashrate" is one MCP call.

**FIX 2 — fluxc quick-compile.** flux-miner is a Flux workspace crate → gets the content-hash cache (FLUXFOOD lever 2), shared target dir (lever 3), mold linker (lever 4). Iterating the miner drops from full-rebuild to ~seconds warm. The miner *is* dogfooded Flux: building the miner proves the toolchain.

**FIX 3 — software auto-updater (the headline ask).** Reuses the `sigil-updater` machinery already built:
```
1. Release author: fluxc compile-native --provenance flux-miner → flux-miner + .proof
2. sigil_updater::ReleaseAnnouncement signed, broadcast on /<chain>/miner-release gossipsub
   (or HTTP-poll a manifest for miners not on P2P)
3. Running miner: every N polls, check manifest → if newer + SQIsign-verified +
   BLAKE3-matched → atomic .new→binary→.bak swap → respawn (the proven flux-arena-agent
   v0.1.5 pattern)
4. No more "wget the new link" ritual. Miners self-upgrade within ~60s of a release.
```
This directly kills CLAUDE.md's manual download-link dance. The miner becomes self-maintaining.

**FIX 4 — provenance-gated mining (anti-botnet bonus).** Because every flux-miner binary ships with a `.proof` (BLAKE3 + SQIsign over source), a chain can *optionally* require that submitted shares come from a provenance-known miner version. Stops a forked/malicious miner from spamming the submit endpoint with malformed proofs. Opt-in per chain.

### Reuse map (don't rewrite)

| Need | Reuse |
|---|---|
| VDF solve + verify | `flux-vdf` (the SIGIL q-vdf port, already in the lock) |
| Auto-update verify+swap | `sigil-updater` (announcement + verify + apply, shipped) |
| Provenance .proof | `fluxc compile-native --provenance` + `flux-sigil` (shipped) |
| Gossipsub release transport | `sigil-net` TOPIC_RELEASE + flux-p2p drain_events (shipped) |
| MCP tool registration | the `fluxc-mcp` handler pattern (shipped) |
| SAP-aware share weighting | `flux-p2p::sap` (a high-SAP miner's shares could get priority — ties into the fee system Lock #23) |

So flux-miner is mostly *integration* of already-shipped pieces + the prover loop. ~600 LOC of new code, the rest is wiring.

### Phase plan

- **P0** — `flux-miner` crate scaffold: prover.rs wrapping flux-vdf, client.rs challenge/submit loop (chain-agnostic endpoint). Mine against Quillon's existing `/api/v1/mining/*` to prove parity with q-miner.
- **P1** — MCP tools (`flux_miner_start/_status/_stop/_tune/_combo`).
- **P2** — auto-updater via sigil-updater (the headline). Self-upgrade demo.
- **P3** — provenance gating + SAP-weighted shares.
- **P4** — SIGIL mining: point flux-miner at SIGIL's mining endpoint (once SIGIL has VDF block production — the Stargate #3 DAG lane). This is "then do the sigil mining" from Viktor.

---

## Part 3 — "then do the sigil mining"

SIGIL doesn't produce blocks via VDF yet (Phase 0 uses `produce-block` CLI; the handoff's Stargate #3 is the DAG lane). The path:

1. flux-miner P0-P2 proves the miner + auto-updater against Quillon (working endpoint to test parity).
2. SIGIL gets VDF block production (port q-mining's `QuantumVDFProof` block-header coupling → `sigil-mining`).
3. flux-miner points at SIGIL's `/sigil/g0/mining/*` endpoints — same miner binary, different chain.
4. The fee system (Lock #23) + mining meet: a miner with high SAP pays near-zero fees AND its shares weight higher. Mining and good behavior compound.

**One miner binary, any Flux chain, self-updating, agent-drivable, provenance-signed.** That's the "exact same system + MCP combo + fluxc + auto-updater" Viktor asked for.

---

## Open questions

1. **GPU path** — q-miner has a `GpuVdfProver` stub. Port it, or CPU-only for v0? (flux-zk already has a GPU path to crib from.)
2. **Auto-update trust root** — single release-author key (like sigil-updater today) or validator-quorum-signed miner releases?
3. **Provenance gating default** — off (permissionless mining) or on (provenance-known miners only)? Lean: off for v0, opt-in per chain.
4. **flux-miner home** — `flux/crates/flux-miner` (substrate, my lean) vs `sigil/crates/sigil-mining` (chain-specific). Substrate wins because Quillon could adopt it too.

— update-v1, 2026-05-30
