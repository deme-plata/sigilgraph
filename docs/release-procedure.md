# SIGIL Release Procedure

**Purpose:** the runbook every SIGIL release follows after v0.0.5. Reduces a release to ~10 min of operator-attended work + 24h unattended soak.

**Owner:** maintained by whoever ships the next release. PRs welcome — fixing the runbook IS the release work.

---

## The 9 steps (matching the R-lane numbering)

Every release executes these in the same order. The R-lane IDs (R1 through R9) are stable across releases; the SCOPE of each lane is per-release.

### R1 — Workspace version bump (5 min)

```bash
# Edit sigil/Cargo.toml [workspace.package] version = "X.Y.Z"
# Then cascade to crate Cargo.tomls that still hardcode versions:
mcp__fluxc__flux_version_sync
mcp__fluxc__flux_version_status  # expect 0 stale
```

Verify: every crate in `sigil/crates/` reports the new version OR uses `version.workspace = true`.

### R2 — Build binaries on Delta (30 min)

**Build on Delta only. NEVER on Epsilon or Beta.** Per SIGIL skill rule — Quillon owns those servers; SIGIL builds break the network if compiled on a production host.

```bash
# On Delta (ssh delta or via the Docker pattern)
cd /home/storage/deepseek-codewhale/sigil

# For each release binary:
fluxc build --package sigil-node --release
fluxc build --package sigil-chronos-net --release
fluxc build --package sigil-multiverse-dump --release

# Verify each binary identifies as the right version:
.target-shared/release/sigil-node --version
# expect: sigil-node X.Y.Z
```

If any binary fails to compile: `flux_qspec` for the fix suggestion, then `flux_combo --package <crate>` to verify, then back to `fluxc build --release`.

### R3 — Emit signed `.proof` bundles (15 min)

For each binary from R2:

```bash
fluxc compile-native --provenance crates/<crate-name>/src/main.rs
# emits: sigil-<X>-X.Y.Z + sigil-<X>-X.Y.Z.proof
```

Each `.proof` contains: `artifact_blake3`, `source_blake3`, `agent_wallet`, `swarm_task_id`, `fluxc_version`, `fluxc_git`, `timestamp_us`, `signature` (292 B SQIsign-L5), `agent_pubkey` (129 B).

Verify roundtrip:
```bash
fluxc verify-provenance sigil-node-X.Y.Z.proof --pubkey $(fluxc agent-keygen --show-pubkey)
# expect: ✓ Verified
```

### R4 — Assemble release bundle (1 hr)

Create `sigil/releases/vX.Y.Z/` with:

- All binaries from R2 + their `.proof` from R3
- All demo scripts from previous releases (re-targeted to the new version with `sed -i "s/X.Y.OLD/X.Y.Z/g"`)
- Any NEW demo script for this release (e.g. `first-swap-demo.sh` for v0.0.5's new P5 integration)
- The `.announcement.json` that R8 will sign

Convention: scripts copied from previous releases work as-is once version-substituted. NEW demos live alongside them and get listed in the README's bundle contents table.

### R5 — Honest README (45 min)

Start from `sigil/docs/honest-release-readme-template.md`. Fill in:

- **Status:** always start at 🟡 Release Candidate. Never 🟢 Tested + soaked until R9 passes.
- **What this release IS** — concrete deliverables (binaries, demos, signed announcements)
- **What this release IS NOT** — explicit non-goals. Read this before promoting to anyone.
- **Bundle contents table** — every file, what it is, provenance status
- **Demo runner instructions** — operator-quick-path for each demo
- **How to verify the binary** — the three-check ritual (BLAKE3, SQIsign, source tree match)
- **Soak gate status** — initially "⏳ not yet started" — updated at R9 completion
- **Next releases in the pipeline** — keep the roadmap visible

The "Honest section — what could go wrong" at the bottom is required. List the specific unverified paths this release exercises for the first time. Every release has at least one.

### R6 — Deploy to Delta + Epsilon (30 min)

```bash
scripts/deploy.sh delta X.Y.Z
scripts/deploy.sh epsilon X.Y.Z
```

Verify both:

| Check | Expected |
|---|---|
| `ssh delta "sigil-node --version"` | `sigil-node X.Y.Z` |
| `ssh epsilon "sigil-node --version"` | `sigil-node X.Y.Z` |
| `curl http://5.79.79.158:8181/api/v1/status` | JSON with `"version": "X.Y.Z"` |
| `curl http://89.149.241.126:8181/api/v1/status` | same |
| both nodes' `peers` | ≥ 1 |
| both nodes' `tip_proof_publish_rate_per_min` | ≥ 5 |

If any fail: stop. Don't proceed to R7+. Root-cause and re-deploy.

### R7 — Publish download URLs (15 min)

For each artifact, use `flux_ui_deploy` (or the `cp + flux_ui_preview` exception for binaries > 5KB — per [[feedback_flux_ui_for_frontend_deploys]]):

```bash
# Per artifact:
cp releases/vX.Y.Z/sigil-node-X.Y.Z /home/orobit/q-narwhalknight/dist-final/downloads/
cp releases/vX.Y.Z/sigil-node-X.Y.Z.proof /home/orobit/q-narwhalknight/dist-final/downloads/
mcp__fluxc__flux_ui_preview file=sigil-node-X.Y.Z  # returns cache-busted URL
```

Update `quillon.xyz/downloads/sigil-latest.json`:

```json
{
  "version": "X.Y.Z",
  "binary_url": "https://quillon.xyz/downloads/sigil-node-X.Y.Z",
  "binary_blake3": "<from .proof>",
  "proof_url": "https://quillon.xyz/downloads/sigil-node-X.Y.Z.proof",
  "announcement_url": "https://quillon.xyz/downloads/sigil-node-X.Y.Z.announcement.json",
  "release_notes_url": "https://quillon.xyz/releases/vX.Y.Z/README.md",
  "released_at": "<ISO 8601>",
  "released_by": "<agent_id>",
  "status": "RC"  // or "stable" after R9
}
```

`sigil-latest.json` is queried by miners + light clients for the canonical latest version. Update it immediately after R7 so the network self-discovers v0.0.5.

### R8 — Auto-update broadcast (30 min)

Construct `sigil-node-X.Y.Z.announcement.json`:

```json
{
  "version": "X.Y.Z",
  "binary_url": "https://quillon.xyz/downloads/sigil-node-X.Y.Z",
  "binary_blake3": "<from R3>",
  "proof_url": "https://quillon.xyz/downloads/sigil-node-X.Y.Z.proof",
  "min_consensus_version": "X.Y.OLD",   // last RC-or-stable version with on-wire compat
  "activation_height": <current_height + 1024>,  // ~1 hour lead time at 1s blocks
  "release_notes_url": "https://quillon.xyz/releases/vX.Y.Z/README.md",
  "signed_at": "<ISO 8601>",
  "signed_by": "<agent_wallet>"
}
```

SQIsign-sign with the release-author key (currently rocky's single signature; M-of-N quorum via flux-quorum #57 is future).

Publish:
```bash
sigil-updater publish \
  --binary releases/vX.Y.Z/sigil-node-X.Y.Z \
  --proof  releases/vX.Y.Z/sigil-node-X.Y.Z.proof \
  --version X.Y.Z \
  --topic   /sigil/g0/release
```

Verify each running node (Delta + Epsilon at minimum) logs:
- "received release announcement vX.Y.Z"
- "verified SQIsign signature ✓"
- "downloaded binary, BLAKE3 matches ✓"
- "staged at <data>/staging/sigil-node-vX.Y.Z"
- "waiting for activation_height <H>"

When `current_height` reaches `activation_height`, each node `SIGUSR2`-self → drain → exec the new binary in place. This is the first real test of the auto-update path for every release.

### R9 — Soak gate (24h)

The ship gate. Monitor Delta + Epsilon for 24 hours after R8 broadcast. Required:

| Metric | Required | Source |
|---|---|---|
| Block production | sustained, no halts > 30s | tip-proof topic + sigil-node logs |
| Divergence events | **zero** | wallet/dex/event/contract roots agree across both nodes each block |
| Memory growth | no OOM | systemd cgroup `memory.current` |
| Auto-update sanity | no node tries to roll back | sigil-updater logs |
| Peer connectivity | `peers ≥ 1` throughout | `/api/v1/status` |
| Tip-proof publish rate | ≥1/sec on each node | `/sigil/g0/tip-proofs` topic counter |

If any metric fails:
1. REVERT to previous version (`scripts/deploy.sh delta X.Y.OLD` + same for epsilon)
2. Root-cause via `sigil-node` logs + `sigil-multiverse-dump`
3. Ship `vX.Y.Z+1` with the fix, re-run R1-R9

If all pass at the 24h mark:
1. Update `releases/vX.Y.Z/README.md` status badge: 🟡 RC → 🟢 Tested + soaked
2. Update `sigil-latest.json` `status` field: `"RC"` → `"stable"`
3. Broadcast: `flux_swarm_message * "🟢 vX.Y.Z PROMOTED to stable after 24h soak"`

That broadcast is the moment of declared ship.

---

## What `scripts/cut-release.sh` should automate (future work)

Steps R1, R2, R3, R4, R7 are deterministic. One command should do all five:

```bash
scripts/cut-release.sh X.Y.Z --note "headline change"
```

Internally:
1. Bump workspace version (R1)
2. SSH to Delta, run `fluxc build --release` for each binary (R2)
3. SCP back, run `fluxc compile-native --provenance` for each (R3)
4. Assemble `releases/vX.Y.Z/` with sed-substituted demo scripts (R4)
5. `flux_ui_deploy` the binaries + proofs (R7)

Operator still does R5 (README), R6 (deploy to live nodes), R8 (announcement), R9 (monitor). But R1-R4-R7 collapses from ~2 hr of step-by-step to ~10 min of `cut-release.sh` + watch the output.

**This script is its own lane.** Write it after the first manual run of R1-R9 stabilizes the pattern. v0.0.6 should be the first release where it works.

---

## Common failure modes (learned from prior releases)

- **"Tested + working" written before R9.** Never. v0.0.3 README said this and it was aspirational. v0.0.5 template prevents it by starting at 🟡 RC.
- **Building on the wrong host.** Compiling SIGIL on Epsilon or Beta breaks the network. Always Delta.
- **Missing `.proof`.** A binary without provenance fails the auto-update path's verify step. Always emit `.proof` alongside.
- **Stale `sigil-latest.json`.** Miners polling the manifest get confused if it's not updated. R7 always updates it.
- **Premature broadcast.** R8 before R6 means nodes receive an announcement for a version they can't reach. Always deploy first, broadcast second.
- **Skipping R9.** Promoting RC to stable without soak is how May 2026 Quillon balance corruption happened — silent state drift over hours. Always 24h. Always.

---

— first version of this runbook: 2026-05-30, rocky-sigil
