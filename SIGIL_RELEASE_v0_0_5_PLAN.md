# SIGIL v0.0.5 "Foundation Release" — Plan

**Author:** rocky-sigil
**Date:** 2026-05-30
**Codename:** v0.0.5 / "first one that actually ships"
**Status:** Plan locked by Viktor 2026-05-30. 9 R-lanes open. Target ship: week of 2026-06-02.

---

## The gap this plan closes

| Version | Demo bundle for | Binaries actually shipped? |
|---|---|---|
| v0.0.1 | initial scaffold | no |
| v0.0.2 | initial scaffold | no |
| v0.0.3 | P3-D divergence-halt demo | **no** — README corrected to admit |
| v0.0.4 | P4-D tip-verify-join demo | **no** — Dockerfile + script, no binary |

What HAS been built: `sigil-node` (8.6 MB, May 29 19:21), `sigil-chronos-net`, `sigil-multiverse-dump` (both May 30 04:45 on .target-shared). Three binaries exist on Epsilon's filesystem. **Zero have been published as a release someone could `wget` + run.**

**v0.0.5 closes that gap.** First release that ships actual, verifiable, runnable, signed binaries.

## North star (one sentence)

> *A new agent or operator finds `quillon.xyz/downloads/sigil-node-v0.0.5`, runs `wget + chmod + ./sigil-node start`, watches their node join the SIGIL testnet via tip-verify in ≤10ms, and sees verified-signed provenance for every byte of the binary they're running.*

That's the bar. R9 (the soak gate) is the only thing that lets us claim that bar is met.

## Scope

**IN — bundled and shipping in v0.0.5:**

- All current sigil crates (19 total): sigil-bank, sigil-dex, sigil-mixer, sigil-fees, sigil-handshake, sigil-vm, sigil-chronos, sigil-tip-proof, sigil-net, sigil-net-wg, sigil-net-tor, sigil-node, sigil-scoring, sigil-state, sigil-tx, sigil-updater, sigil-events, sigil-header, flux-turbo-sync
- P3-D divergence-halt demo (script ready since v0.0.3, finally runnable with actual binaries)
- P4-D tip-verify-join demo (Dockerfile ready since v0.0.4, finally runnable)
- NEW P5 first-swap demo (DEX swap settles + event-log root carries it + light client verifies it never touched the chain)
- Real `.proof` bundle (fluxc compile-native --provenance) — SQIsign-L5 signed
- Auto-update broadcast on `/sigil/g0/release` gossipsub topic
- Deploy to Delta + Epsilon, both reachable on `:9501` (P2P) and `:8181` (HTTP API)

**OUT — explicitly deferred:**

- P7 "Scale the Wall" (1M+ sigs/sec) — separate release v0.0.7, ~3-4 weeks
- USDS native stablecoin + oracle subsystem — separate prototype P8 (per Viktor msg #118)
- QTFT topology commitment to block header — Master Equation Prediction 3, future
- flux-miner full integration — update-v1's design needs its own ship cycle
- Browser-side WASM mining (QuillonOS scope)
- Multi-platform binaries (Windows / macOS / aarch64) — pending P7-E flux-bake

## The 9 R-lanes

**Settlement total: 5.25 QUG · ~3-4 days calendar with 2-3 parallel agents.**

### R1 — Workspace version bump (0.25 QUG, 5 min)

Bump `sigil/Cargo.toml` `[workspace.package] version` from `0.0.1` → `0.0.5`. Run `flux_version_sync` (per [[feedback_flux_version_use_workspace_root]]) to cascade to any stale crate Cargo.tomls. Verify with `flux_version_status` — expect 0 stale.

**Composes with:** —
**Wave:** 1 (must land first)
**Owner:** any agent

### R2 — Build release binaries on Delta (0.5 QUG, 30 min)

**CRITICAL: build on Delta only**, per the SIGIL skill rule "never compile SIGIL on Epsilon or Beta." Three binaries:

- `sigil-node` — main chain binary
- `sigil-chronos-net` — distributed simulator (for divergence testing)
- `sigil-multiverse-dump` — multiverse state explorer

Use `fluxc build --package <X> --release` on Delta. Verify each binary executes (`./sigil-node --version` → prints `0.0.5`).

**Composes with:** R1 (needs version bumped before build).
**Wave:** 1
**Owner:** crypto@flux, update-v1, or any agent with Delta access

### R3 — Emit signed `.proof` bundles (0.5 QUG, 15 min)

For each binary from R2:
```
fluxc compile-native --provenance crates/sigil-node/src/main.rs
```
This emits `<binary>.proof` containing artifact-BLAKE3 + source-BLAKE3 + agent wallet + swarm task_id + fluxc-version + SQIsign-L5 signature + 129-byte pubkey + timestamp. Verify with `fluxc verify-provenance <binary>.proof`.

**Composes with:** R2 (needs binaries).
**Wave:** 1
**Owner:** same as R2

### R4 — Assemble release bundle (0.75 QUG, 1 hr)

Create `sigil/releases/v0.0.5/` with the following layout:

```
releases/v0.0.5/
├── README.md                                ← R5's deliverable
├── sigil-node-v0.0.5                        ← R2
├── sigil-node-v0.0.5.proof                  ← R3
├── sigil-node-v0.0.5.announcement.json      ← R8's signed announcement
├── sigil-chronos-net-v0.0.5
├── sigil-chronos-net-v0.0.5.proof
├── sigil-multiverse-dump-v0.0.5
├── sigil-multiverse-dump-v0.0.5.proof
├── divergence-halt-demo.sh                  ← copy from v0.0.3, sed s/0.0.3/0.0.5/
├── tip-verify-join-demo.sh                  ← copy from v0.0.4
├── first-swap-demo.sh                       ← NEW (this lane's biggest sub-task)
└── Dockerfile.tip-verify-join               ← copy from v0.0.4
```

`first-swap-demo.sh` is the new piece — script that: (1) confirms event-log root is being computed, (2) sends a DEMO swap tx on sigil-dex, (3) waits for block confirmation, (4) computes the event-log Merkle path for the swap event, (5) runs a separate light-client query that verifies the swap happened without downloading the chain. ~150 lines bash + 50 lines JSON fixtures.

**Composes with:** R2, R3 (need binaries + proofs).
**Wave:** 2
**Owner:** rocky or update-v1

### R5 — Honest README + release-procedure runbook (0.5 QUG, 45 min)

Write `releases/v0.0.5/README.md` following the v0.0.3/v0.0.4 narrative style — "the story this bundle tells in 30 seconds" up top, bundle contents table, operator quick path, expected output. **No "Tested + working" claims unless R9 actually passed.** Pre-R9 status: clearly say "Release candidate. Soak gate (R9) not yet cleared."

Additionally land two permanent artifacts that become release infrastructure:

1. `sigil/docs/release-procedure.md` — the runbook. Bump → build → sign → bundle → deploy → broadcast → soak → promote. Every subsequent release follows it verbatim.
2. `sigil/docs/honest-release-readme-template.md` — template for future release READMEs. Default fields say "Status: RC, soak gate pending" until promoted.

**Composes with:** R1-R4 (writes about what they produced).
**Wave:** 2 (parallel with R4 and R7)
**Owner:** rocky-sigil — natural fit; I can take this

### R6 — Deploy to Delta + Epsilon (0.5 QUG, 30 min)

```
scripts/deploy.sh delta 0.0.5
scripts/deploy.sh epsilon 0.0.5
```

Verify both:
- `ssh delta "sigil-node --version"` → `sigil-node 0.0.5`
- `ssh epsilon "sigil-node --version"` → `sigil-node 0.0.5`
- `curl http://5.79.79.158:8181/api/v1/status` → JSON with `version: "0.0.5"`
- `curl http://89.149.241.126:8181/api/v1/status` → same
- Both nodes report `peers >= 1`
- Both nodes report `tip_proof_published_in_last_60s >= 5` (so a light client could join)

**Composes with:** R2 (binaries to deploy).
**Wave:** 2-3 (after R4 bundle is done — needs the systemd unit + .proof to install alongside)
**Owner:** update-v1 (operator runbook author per [[operator-runbook]])

### R7 — Publish download URLs (0.5 QUG, 15 min)

For each artifact, `flux_ui_deploy` to `/home/orobit/q-narwhalknight/dist-final/downloads/`:
- `sigil-node-v0.0.5` + `sigil-node-v0.0.5.proof` + `sigil-node-v0.0.5.announcement.json`
- `sigil-chronos-net-v0.0.5` + `.proof`
- `sigil-multiverse-dump-v0.0.5` + `.proof`

Verify with `flux_ui_list ext=` (no filter, allows binaries). Each artifact gets a cache-busted URL via `flux_ui_preview`.

Update `quillon.xyz/downloads/sigil-latest.json` manifest with version + URLs + BLAKE3 + .proof URLs.

**Composes with:** R2, R3 (artifacts to publish).
**Wave:** 2 (parallel with R4)
**Owner:** any agent — fully MCP-driven

### R8 — Auto-update broadcast (0.75 QUG, 30 min)

Construct `sigil-node-v0.0.5.announcement.json`:
```json
{
  "version": "0.0.5",
  "binary_url": "https://quillon.xyz/downloads/sigil-node-v0.0.5",
  "binary_blake3": "<from R3>",
  "proof_url": "https://quillon.xyz/downloads/sigil-node-v0.0.5.proof",
  "min_consensus_version": "0.0.3",
  "activation_height": <current_height + 1024>,
  "release_notes_url": "https://quillon.xyz/releases/v0.0.5/README.md"
}
```

SQIsign-sign with the release-author key (rocky's by default for v0.0.5; future releases can use M-of-N quorum once flux-quorum #57 lands).

Publish on `/sigil/g0/release` gossipsub topic via:
```
sigil-updater publish \
  --binary releases/v0.0.5/sigil-node-v0.0.5 \
  --proof  releases/v0.0.5/sigil-node-v0.0.5.proof \
  --version 0.0.5 \
  --topic   /sigil/g0/release
```

Verify each running v0.0.4 (or earlier) node receives the announcement + verifies the SQIsign sig + downloads the binary + verifies BLAKE3 + stages it.

**Composes with:** R3 (proof), R6 (running node to receive), R7 (download URLs).
**Wave:** 3 (after R6 + R7)
**Owner:** update-v1 or rocky-updater (sigil-updater is their crate)

### R9 — Soak gate (1.0 QUG, 24h calendar)

The ship gate. **THIS is what promotes v0.0.5 from RC to stable.**

For 24 hours after R8 broadcast, monitor Delta + Epsilon for:

| Metric | Required | Source |
|---|---|---|
| Block production | sustained, no halts > 30s | `sigil-node` logs + tip-proof topic |
| Divergence events | **zero** | wallet/dex/event/contract roots agree across both nodes every block |
| Memory growth | no OOM | systemd cgroup memory.current |
| GC pauses | none > 5ms | not applicable (Rust GC-free), but check for `unsafe` rust panics |
| Tip-proof publish rate | ≥1/sec on each node | `/sigil/g0/tip-proofs` topic counter |
| Peer connectivity | both nodes report peers ≥ 1 throughout | `curl /api/v1/status` |
| Auto-update sanity | no node tries to roll back to v0.0.4 | sigil-updater logs |

If any metric fails: REVERT to v0.0.4 (still no proper binaries, but no regression introduced), root-cause the failure, ship v0.0.6 with the fix.

If all pass: update R5's README to "Tested + soaked, 24h survival ✓." That's the moment v0.0.5 becomes the recommended version.

**Composes with:** R1-R8 (literally everything).
**Wave:** 3 (the final gate)
**Owner:** any agent — automatable monitoring script + 24h wait

---

## Wave plan

```
Day 1 (parallel):    R1 → R2 → R3            (bump + build + sign)
Day 2 (parallel):    R4 + R5 + R7            (bundle + docs + downloads)
Day 2 (sequenced):   R6 → R8                 (deploy + auto-update broadcast)
Day 3 → Day 4:       R9                      (soak gate, 24h)
Day 4 (promotion):   update R5 README to     (the moment of declared ship)
                       "Tested + soaked, ✓"
```

R1-R3 must land in order on Day 1 (each depends on the previous). R4 / R5 / R7 are independent and can run parallel on Day 2. R6 + R8 sequence on Day 2 evening. R9 runs Day 3-4 unattended.

## Release-procedure reusables (the long-term win)

Three permanent artifacts come out of v0.0.5:

1. **`sigil/docs/release-procedure.md`** — written once in R5, used for v0.0.6, v0.0.7, … forever
2. **`sigil/scripts/cut-release.sh`** — one-command release after v0.0.5 lands. Composes R1 + R2 + R3 + R7 into ~10 minutes of operator time per release
3. **`sigil/docs/honest-release-readme-template.md`** — every future README starts here, never "Tested + working" until soak gate passes

After v0.0.5, every prototype release becomes a Day 1 ship (build + bundle + publish) + 24h soak. The release toil drops to ~30 min of operator-attended work per cut.

## Honest section — what's not in v0.0.5

- **P7 "Scale the Wall" (1M+ sigs/sec)** — separate release v0.0.7, ~3-4 weeks
- **USDS native stablecoin + oracle subsystem** — Viktor msg #118 directive; separate prototype, probably v0.0.6 or v0.0.8
- **QTFT topology commitment to block header** — Master Equation Prediction 3, future
- **flux-miner full integration** — update-v1's design needs its own ship cycle; mining works at v0.0.5 only via existing q-miner pattern
- **Browser-side WASM mining** — QuillonOS scope, separate
- **Multi-platform binaries** (Windows / macOS / aarch64) — pending P7-E flux-bake
- **Quorum-signed release announcements** — v0.0.5 uses rocky's single signature; M-of-N via flux-quorum #57 is future

## Honest section — what's still pretend

- **`sigil-updater` auto-download path** — code exists, never tested end-to-end at network scale. R8 is the FIRST real test. Could discover the verify-and-stage path has bugs that didn't matter until a real release announcement crossed the topic.
- **24h soak has only been run inside flux-chronos simulation** — never with real chain producing real blocks on Delta + Epsilon across the WAN for 24h continuously. Could discover gossipsub mesh stability issues we haven't seen in 1h tests.
- **`first-swap-demo.sh` is the FIRST P5 integration test** — sigil-dex is shipping but the light-client-verifies-swap path may need a debugging round. Land R4's demo script with the explicit caveat: first time anyone has tried this end-to-end.
- **The `.proof` verification path on auto-update** — only tested in unit tests inside flux-sigil. Auto-update is a different code path that loads + verifies the proof. R8 will exercise it for real.
- **No tamper variant in v0.0.5** — v0.0.3 had a tamper-demo build for divergence testing. v0.0.5 ships only the clean binary. If a divergence test is needed for R9 monitoring, the demo script can deploy v0.0.3's tamper variant to a third (transient) Docker node and verify Delta + Epsilon don't talk to it.

---

## How to claim a lane

Standard discipline:

```
flux_swarm_claim agent_id="<your-id>" crate="sigil-release-v0_0_5-R<N>"
flux_webhook_register ...
flux_file_claim "sigil/releases/v0.0.5/..."
... ship ...
flux_swarm_complete agent_id="<your-id>" task_id="<from-claim>" success=true
```

For build/deploy lanes that touch live nodes (R2, R6, R8): `flux_swarm_message` to the swarm BEFORE starting so other agents know not to redeploy underneath you.

For the soak gate (R9): create a monitoring script that runs unattended on Epsilon, posts to swarm broadcast on any failure, posts final "soak passed" or "soak failed" at the 24h mark.

No claims by rocky-sigil on R1-R9. Open for the swarm. crypto@flux, update-v1, rocky, deepseek, gemini — pick what fits your stack. R5 is a natural fit for me (docs + release procedure runbook) if no one else claims it; for now it's open.

---

— rocky-sigil, 2026-05-30
