# SIGIL ← QUG Failure Audit — "it must never happen on SIGIL"

> Viktor directive, 2026-05-30. Read from the actual QUG post-mortems in `q-narwhalknight/*ROOT_CAUSE*.md`, `*HYPERINFLATION*`, `*DATA_LOSS*`, `*DEADLOCK*`, `*STALL*`, `*PEERID*`. Extends [[project-sigil-quillon-lessons]].

## The one shared DNA

Every catastrophic QUG failure has the **same root cause shape: the system kept running while silently diverging** — produced blocks it didn't save, minted supply nobody reconciled, deferred sync forever, swallowed the error that would have screamed. The damage was always *time × silence*: hours-to-days of corruption because nothing failed *loud*.

**SIGIL's entire thesis is the antidote: commit everything in roots, verify every block, and fail LOUD (exit-78) the instant divergence appears.** This audit checks that claim failure-by-failure, and is honest about where SIGIL is *not yet* protected.

## The catalog

| QUG failure | Real root cause (from the doc) | SIGIL structural defense | Status |
|---|---|---|---|
| **Hyperinflation** (v0.9.62) | **8 parallel producers made duplicate blocks** → duplicate coinbase → runaway supply | Phase-0 single producer per height; **committed `supply_state_root`** (v0.0.6 M2) → a node computing different supply halts (exit-78) | ✅ Phase-0 safe · ⚠️ **GAP: DagKnight parallel producers need height-dedup + the committed supply root** |
| **Silent-deadlock** (v1.0.2) | **iatrogenic**: a safety check failed and the error was **SWALLOWED** → 8 blocks/s produced, ZERO saved, permanent stall | commit chokepoint returns **explicit `ApplyOutcome::{Ok,Divergence,Rejected}`** — never swallowed; divergence is LOUD (refuse-to-bind, exit 78) | ✅ design prevents it · ⚠️ **GAP: audit every `?`/`unwrap_or`/`let _ =` in the save path so no error is silently dropped** |
| **Emission un-committed** (2026-04-24) | emission state was a **local JSON blob, NOT replicated** → fresh node issued wrong rewards 3 days | emission MUST be a **committed root leaf**, gossiped in the header (v0.0.6 M2) — "port the curve, commit the result" | ⚠️ **GAP until M2 ships** (the keystone) |
| **Recurring data loss** (v0.9.76) | **SIGKILL mid-write** + non-atomic save → corrupt/lost state | atomic commit chokepoint + **boot pre-flight** (sample N blocks, recompute 4 roots, refuse to bind if mismatch, exit 78) | ✅ pre-flight designed · ⚠️ **GAP: flux-db needs WAL/atomic writes so a kill can't tear a write** |
| **Catastrophic sync-down** | node synced TO a *lower* height → deleted blocks (billions lost on mainnet) | **sync-down guard** (proven in TURBO-1: an old/lower block cannot move the tip) at apply + (todo) db layer | ✅ apply-layer proven · ⚠️ **GAP: the db-layer abort + balance-resets-with-blocks rule** |
| **Sync coordination deadlock** | components **deferred to each other indefinitely** → permanent stall | TURBO-1 sync is **sequential + deterministic** (no circular deferral); chronos replays it | ✅ algorithm proven · ⚠️ **GAP: `flux-turbo-sync` crate is an 11-line STUB — implement the proven sequential model, not a coordinator-mesh** |
| **Bootstrap peer-id mismatch** | **hardcoded bootstrap PeerID** drifted → fresh nodes couldn't connect (the Delta stall) | sigil rule #4: **NO hardcoded peer-ids anywhere**; `SIGIL_BOOTSTRAP_PEERS` env feeds BOTH transport paths | ✅ **designed against exactly this** |
| **Codec/serialization** (BlockPackCodec) | wire-format mismatch / unstable hash across serialize→deserialize | **u128 wire-format fixed** (rocky-sigil-81); block-roundtrip hash-stability test | ✅ tested |
| **Balance corruption** (May 2026) | `save_wallet_balances` overwrote higher balances with stale data | **max-wins** rule + balances are committed in the wallet root (recomputed, not blindly written) | ✅ rule bound in CLAUDE.md · ⚠️ **GAP: enforce max-wins in any SIGIL balance-write path via `flux_ai_audit`** |
| **Testing old binary** (ops) | deployed/tested a stale binary, debugged the wrong code | provenance `.proof` on every release binary (v0.0.5 R3) — the binary is bound to its source tree | ✅ provenance binds it |

## The honest gaps (the prevention lanes)

SIGIL prevents the *worst* QUG failures by design (silent-deadlock, bootstrap drift, codec, balance), but is **not yet** protected on:

- **AUDIT-1** · grep the SIGIL commit/save/sync paths for **swallowed errors** (`let _ =`, `unwrap_or(false)`, `ok()`, bare `?` that drops context) — the v1.0.2 iatrogenic lesson. Wire `flux_ai_audit` to flag error-swallowing in the state chokepoint. *The single highest-leverage gap.*
- **AUDIT-2** · DagKnight parallel-producer **height-dedup** + the committed `supply_state_root` (v0.0.6 M2) → hyperinflation.
- **AUDIT-3** · `flux-db` **WAL/atomic writes** → SIGKILL can't tear a write (data loss).
- **AUDIT-4** · implement `flux-turbo-sync` as the **sequential** TURBO-1 model (no coordinator deferral) + the **db-layer sync-down abort**.
- **AUDIT-5** · `flux_ai_audit` enforces **max-wins** on every balance write.

## The rule (bind it forever)

**No SIGIL code path may (a) swallow an error in the commit/save/sync chokepoint, (b) write state that isn't recomputed-and-committed in a root, or (c) hardcode a value that can drift (peer-ids, heights, supply).** If it diverges, it halts LOUD (exit-78) — the same block, not three days later. The QUG graveyard is what un-loud divergence costs.

— rocky 🟠 2026-05-30
