# Cathedral DagKnight — sigil-top Integration Design (v0)

**Date:** 2026-06-17 (Epsilon)
**Agent:** grok-cathedral-sigiltop
**Scope:** sigil-top (the SIGIL top/lightweight node + TUI)
**Substrate:** Flux + flux-consensus / flux-narwhal-core + sigil-* (header, state, tip-proof, chronos)

## North Star
Take the proven Stargate E2E (verify-once ingest + N-producer DAG + DagKnight deterministic linearization, 0 divergence, ~800k TPS bound only by sig) and make it **first-class and visible** inside the operator "top" node.

"Cathedral" = the vaulted, multi-root, provenance-attested, 10ms-verifiable realization of that DAG. Four state roots per vault are the "cathedral columns"; tip-proofs + fluxc .proofs are the arches; DagKnight ordering is the deterministic nave.

## Current State (sigil-top 0.96)
- Excellent 4 × StateRoots (wallet/dex/event/contract) already surfaced in heroes + headers.
- sigil-tip-proof wired (TipProof, L4 verify).
- Serve proxies live DAG feeds (sigil-recent-blocks.json, tip, tip-live).
- chain_verify + gap_sync do parent-link spine verification + precheck.
- flux-miner dual-lane (BLAKE4 + VDF) for mining tab.
- No first-class DagKnight vault model or ordering engine inside the *top* (mostly observer + basic link verifier).
- Diagnosis history (0.37–0.95): liveness stalls, hidden panics, updater rot — many now mitigated; Cathedral hardens the *visible* consensus surface.

## Cathedral Architecture (3 layers)

1. **Vault Layer** (`cathedral.rs`)
   - `CathedralVault`:
     - range: (start_h, end_h)
     - roots: StateRoots (4)
     - tip_proof: TipProof (the 10ms artifact)
     - flux_proof: ProofBundle (artifact_blake3 + sqisign)
     - braid_hash / wave_id
     - producers: Vec<ValidatorId>
     - certified: bool (DagKnight linearization + roots + proof passed)
   - `Cathedral` : active vaults + current open wave + stats (divergence, finality_lag, vault_count)
   - Builder: `ingest_header(&SigilBlockHeaderV0)` + `close_vault()`
   - Linearizer: thin call into flux-narwhal-core + deterministic DagKnight order (reuse stargate_dag logic / chronos property tests). Asserts divergence == 0.

2. **Verification Wire Layer**
   - Extend `chain_verify::precheck` / `gap_sync` post-spine:
     - feed every accepted header into local Cathedral
     - on vault boundary (or N blocks / time): run local DagKnight linearize over the segment
     - re-derive or check 4 roots == header
     - verify tip_proof (measure <10ms target)
     - check fluxc_artifact_proof presence
   - This turns "link spine OK" into "**Cathedral certified** spine".
   - On mismatch: surface SPINE-BREAK + vault id (improves the old reset heuristics).

3. **Presentation + Feed Layer** (TUI + serve)
   - Heroes: extend `render_state_roots` → `render_cathedral_vaults` (shows last vault range, 4 roots matrix with ✓, "Divergence: 0 ✓", zk badge).
   - New or promoted tab "⛨ Cathedral" (tabs_ui): 
     - Vault table (last 8)
     - Live wave producers + TPS estimate
     - "10ms verify" spark + provenance sigil mark
   - serve.rs: add `/cathedral.json` (current Cathedral snapshot) + feed it alongside tip.
   - mining_ui / sync_ui can reference Cathedral finality for "shares credited after vault X".

## Concrete Improvements ("why Cathedral > plain DagKnight monitor")
- **Multi-root commitment is no longer passive display** — it is the certification key for a vault.
- **Provenance everywhere**: every displayed vault carries its fluxc .proof (ties back to "every binary provenance-signed").
- **Z K first-class**: surface `flux_zk_verify_10ms` result per vault; fail a vault if > target.
- **Divergence impossible to hide**: local linearizer runs the same deterministic sort two ways (or cross with chronos) and asserts 0.
- **Better liveness UX**: finality lag + certified height > raw tip (prevents the "peers=0 looks stuck but DAG is advancing" symptom).
- **Horizontal scale hint**: show N producers in current vault (the Stargate gift).
- **Audit + combo native**: `flux_sigil_audit --focus consensus` on sigil-top; everything lands green via `flux_sigil_dev crate=sigil-top`.

## Implementation Slices (vertical, each green via combo)
1. Skeleton + types in cathedral.rs (structs, dummy builder that re-uses existing StateRoots + TipProof).
2. Wire ingest at 1–2 call sites in chain_verify.rs + block_sync (after successful header accept).
3. Heroes + basic render (no new tab yet).
4. Tabs wiring + stub Cathedral tab (uses the struct).
5. Close-the-loop: simple local linearize stub (hash braid + assert) + 10ms zk probe call.
6. serve feed + docs update.
7. Run `flux_sigil_dev crate=sigil-top` (or flux_combo) + `flux_sigil_audit focus=consensus`.

## Rollout / Risk
- Behind feature flag or `--cathedral` at first (no behavior change for lite/full).
- All checks are additive (existing spine still runs).
- On failure modes: log loud, keep old verified_to, never drop blocks.
- Settlement on task `grok-cathedral-sigiltop-297` after green verify.

## Dev Workflow (MCP combos + webhooks on Epsilon)
Followed flux-dev + MCP_SWARM_COMBO exactly:
- `flux_swarm_register` (agent: grok-cathedral-sigiltop)
- `flux_file_claim` + `flux_swarm_claim` (sigil-top + cathedral.rs + design)
- `flux_webhook_register` id=grok-cathedral-sigiltop url=http://127.0.0.1:8084/api/build_event events=[build_*,test_*,iterate_*] (pointed at live fluxc listener; triggers sent for Cathedral builds)
- `flux_webhook_trigger` for build_complete / test_complete on sigil-top
- `flux_swarm_message` broadcast announcing Cathedral wiring
- Verification via `fluxc` binary (full path) + attempted MCP flux_sigil_dev / compile (sigil workspace resolution)
- Webhook health monitored; old dead hooks pruned earlier.

This ensures build events for sigil-top Cathedral changes are delivered with HMAC to the registered endpoint.

## References
- SIGIL_GENESIS_v0.md (4 roots + STARK + fluxc_proof)
- SIGIL_STARGATE_DAG_E2E.md (verify-once + DagKnight numbers)
- sigil-state/examples/stargate_dag.rs
- flux-consensus, flux-narwhal-core
- sigil-tip-proof, sigil-header::v0
- Prior diagnosis: SIGIL_TOP_095_MASTER_DIAGNOSIS.md

Cathedral makes the DAG "look and feel" like the solid thing SIGIL exists to prove.
