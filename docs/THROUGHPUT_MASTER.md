# SIGIL Fast-Sync Throughput — MASTER SPEC
## Break the flux-db durable-commit wall (~4k → ≥92.6k blk/s)

**Audience:** ChatGPT Codex (implementer of record) + 4 collaborating terminal sessions (lane owners).
**Repo:** `/home/storage/deepseek-codewhale/sigil` · branch `feat/commons-tithe-routing-0.36.1`
**flux-db (path dep):** `/home/storage/deepseek-codewhale/flux/crates/flux-db` (sibling repo)
**Date:** 2026-06-23 · **Status of tree:** secured/green at commits `cd31bf7..b62fafc` (unsigned, local).

---

## 0. MISSION (read this first)

The light-client **skeleton** fast-sync already works: a flat, fixed-stride 72-byte append store
(`flux_db::skeleton::SkeletonStore`) bulk-imports the verified prefix at ~10M rec/s. That is NOT the
problem and must not be touched except at its seams.

The problem is **full block BODIES**. Every verified frontier/gossip block is persisted to flux-db's
**custom LSM key-value store** (NOT RocksDB) via the `CommitBuffer` ring. That durable KV path caps at
**~3,863 records/s** (`flux/crates/flux-db/src/skeleton.rs:9`) — the "5k cap". It is why end-to-end
sync is runtime-proven at **39,112 blk/s** while the chronos transport ceiling is **92,600 blk/s**
(`crates/sigil-top/src/block_sync/commit.rs:41,63`).

**GOAL:** lift the durable full-body commit path from ~4k to **≥92,600 blk/s sustained**, genesis→tip,
**divergence = 0**, with **kill-9 crash-safety preserved**, **without changing the consensus block hash**,
and **without weakening verification**. Everything ships **DARK-by-default behind env flags** — zero
change to field-operator behavior until a flag is set.

---

## 1. GROUND TRUTH — current architecture (cited)

- **flux-db is a custom LSM, not RocksDB.** Single default column family; WAL → memtable → leveled SST,
  LZ4 FAST(1) on SST bodies. No `Options`/`WriteOptions` struct; tunables are constants + setters in
  `flux/crates/flux-db/src/lib.rs`.
- **Block store:** `crates/sigil-top/src/block_store.rs`. Three write paths:
  - `put_block` (`:464`) — slow path: **2 separate `db.put`/block** (`:497-498`) + read-before-write
    (`linkage_conflict`→`get_stored_at_height(h-1)` = 2 gets incl. a bincode body decode, `:270,:458,:833`).
  - `put_blocks_batch` (`:547`) — one `db.batch_put` (`:651`) but still a per-block `height_index_conflict`
    get in the assembly loop (`:605`). Comment `:664`: "capped commit at ~3.3k blk/s".
  - `put_blocks_bulk_trusted` (`:674`) — **zero per-block gets**, one `batch_put` (`:710`). Requires a
    verified/contiguous/fold-proven prefix. **Default OFF** (`SIGIL_COMMIT_TRUSTED=false`, `commit.rs:102`).
- **Durable sink / group-commit:** `block_sync/commit.rs::CommitBuffer` (`:111`). Batch = `SIGIL_COMMIT_BATCH`
  (default **16384**, clamp 256..1048576). fsync **once per drained batch** (2-phase: blocks durable →
  tip 'S' → tip durable; `block_store.rs:813-820`). Driven by `block_sync/mod.rs::launch()`:
  ring built `mod.rs:651`, fed `push_slice` `:898`, drained `flush` `:985`, bulk-load armed/folded `:989-990`.
- **Skeleton prefix (DONE):** flat `flux_db::skeleton::SkeletonStore` (72-B records `height‖block_hash‖parent_hash`,
  O(1) `read_at`, 2-phase durable, torn-tail truncate). Commit `98f0449` consolidated SkeletonStore ONTO this
  flat engine (it did NOT route it back through the LSM). Live: skeleton store ~1.34M records, main store ~1.0k.
- **Serialization:** bodies are **bincode** (`block_store.rs:496,531,572,690`). BUT `SigilBlockHeaderV0::hash()`
  **JSON-serializes the whole ~1 KB header** (~3-5 KB text, ~15-25 µs/call; `block_store.rs:451-456,549-554`).
  v0.33 already switched parent-linkage to a stored-hash 32-byte memcmp; hash() call-frequency is reduced but
  the per-call JSON cost remains.

---

## 2. ROOT-CAUSE BOTTLENECK RANKING (the targets)

**#1 — Synchronous leveled-compaction write-amplification at the 64 MB WAL auto-flush.**
A forward sync is append-mostly, so mid-sync leveled compaction is ~pure write-amp (ratio 4/level → 10-20×
ingest bytes), fired synchronously inside `flush()` (`flux-db/src/lib.rs:1167-1170`). This is the real
**>20k blk/s wall** (`lib.rs:135`, `commit.rs:48`), batch- and fsync-independent.
*Partially mitigated:* `defer_compaction` + 256 MB memtable + `BULK_L0_COMPACT_THRESHOLD=32` + `compact_to_tip`
at tip (`block_store.rs:748-758`). **Not eliminated.**

**#2 — Per-block read-before-write (guaranteed-miss gets) on the checked path.**
`block_store.rs:664-665` (~3.3k cap); `linkage_conflict` does 2 gets/block incl. a bincode decode.
*Lever:* default the **trusted zero-get path** for the fold/anchor-verified prefix + add an in-memory
**parent-hash cache** for the checked frontier. Trusted path exists (`:674`) but is **OFF by default**;
a real parent-hash cache for the checked path is **absent**.

**#3 — No bulk SST ingest for bodies; no cross-CF atomic WAL; per-put userspace flush.**
flux-db has **no `IngestExternalFile`** equivalent for full bodies (the flat skeleton store is the only
bulk-ingest engine, and it only holds 72-B records, not bodies). `WriteBatch::write` exists (`lib.rs:713`)
but block_store uses single-CF `batch_put`; each `write_wal_entry` does a userspace `wal.flush()` (`lib.rs:1303`).
*Lever:* **build sorted SST files off-thread and atomically ingest the verified body prefix** (near-disk speed),
+ a true group-commit WAL. **Absent — the single biggest unbuilt win.**

**#4 — Redundant bincode round-trips + JSON `header.hash()`.**
`fetch.rs:436-440` ("3 bincode round-trips/record: deserialize + verify-encode + commit-encode");
`header.hash()` JSON cost (`block_store.rs:451-456`).
*Lever:* collapse to 1 codec pass; **cache the computed hash on `StoredBlock`** so it is computed once and
never recomputed (do NOT change the hash definition — see §5). Parallel hash/serialize across cores already
done (`:556-579`); redundant encodes on the body path are not yet collapsed.

---

## 3. SOLUTION DESIGN — the levers, prioritized

| Pri | Lever | Where | Expected ceiling lift |
|----|-------|-------|----------------------|
| P0 | **SST-ingest the verified body prefix** (build sorted SSTs off-thread, atomic ingest, skip WAL+memtable+compaction for the trusted prefix) | flux-db + block_store | ~disk speed (10-100×) |
| P0 | **Trusted zero-get path ON for fold/anchor-verified prefix** + parent-hash cache for checked frontier | block_store + commit | kills the ~3.3k get-wall |
| P1 | **Bulk-load compaction discipline**: fully defer during catch-up, manual compact at tip, add a write-stall/backpressure guard | flux-db | removes the >20k write-amp wall |
| P1 | **Group-commit WAL** (one fsync + one syscall per large batch, no per-put userspace flush) + optional header/body CF split | flux-db | removes per-put + fsync overhead |
| P2 | **Codec collapse**: 1 bincode pass end-to-end; cache `StoredBlock.hash` (compute once) | block_store + fetch/verify | frees CPU for verify |
| P2 | **Staged pipeline**: decode→verify(rayon)→commit on a bounded channel, IO-only commit thread, backpressure | block_sync/mod.rs | hides commit latency |

DARK-by-default flags (extend the existing `SIGIL_COMMIT_*` family): `SIGIL_DB_SST_INGEST`,
`SIGIL_COMMIT_TRUSTED` (flip default for verified-below), `SIGIL_DB_DEFER_COMPACT`, `SIGIL_COMMIT_PIPELINE`.

---

## 4. WORK DECOMPOSITION — 4 sessions + Codex

Each **terminal session owns one lane**: produces the design + a reference patch in its scoped files,
following seam discipline. **Codex is the implementer of record**: consolidates, lands the code on all
lanes, keeps it compiling green on the build host, wires the flags, and runs the bench. A session that
cannot build hands its patch/spec to Codex.

| Lane | Owner (session) | Scope | Files (claim these) | Seam |
|------|-----------------|-------|---------------------|------|
| **L1 — flux-db bulk engine** | Session 1 | SST off-thread build + atomic ingest for verified body prefix; full compaction-defer + manual compact-at-tip; write-stall guard | `flux/crates/flux-db/src/{lib.rs,cf.rs,sst.rs,skeleton.rs}` | exposes `ingest_sorted_bodies()` + `bulk_mode()` API consumed by L2 |
| **L2 — commit path** | Session 2 | Default trusted zero-get path for fold/anchor-verified prefix; parent-hash cache for checked frontier; wire L1 ingest API | `crates/sigil-top/src/block_store.rs`, `block_sync/commit.rs` | consumes L1 API; commit-hook called by L4 launch() |
| **L3 — codec & hashing** | Session 3 | Collapse 3→1 bincode round-trips; cache `StoredBlock.hash` (compute once, never recompute); NO hash-definition change | `block_store.rs` (codec only), `block_sync/{fetch.rs,verify.rs}`, `crates/sigil-header` | shares `StoredBlock` struct with L2 — coordinate the cached-hash field |
| **L4 — pipeline & bench** | Session 4 | Bounded-channel decode→verify→commit pipeline + IO-only commit thread; reproducible blk/s bench + virtual-time rig (the acceptance gate) | `block_sync/mod.rs` (launch loop), `crates/sigil-state/tests/`, a bench bin | owns launch() drain ordering; calls L2 commit ring |
| **INTEGRATE** | **Codex** | Land all lanes, resolve seams, green build on the build host, wire flags DARK, run the bench, report blk/s | all of the above | owns final merge order |

**Coordination protocol (use the flux swarm bus):**
- Claim files before editing: `flux_file_claim` (or `flux_file_list` to check). `block_sync/mod.rs` is a
  hot shared region — L4 owns the launch() drain; L2 only adds a commit-hook call-site.
- Seam changes (API surface between lanes) → broadcast via `flux_swarm_message` BEFORE landing.
- Snapshot work often: `flux_swarm_snapshot`. Commit early + often (this tree has lost uncommitted work before).
- **Build discipline:** build via `flux-cargo-wrapper` with `ionice -c3 nice -n 19`, `TMPDIR`/`CARGO_HOME`/
  `target` redirected to `/home/storage` (root disk is 40 G / ~91% full). **No builds on the prod/ssh-critical
  box during the merge; no concurrent compiles.** Build host = the designated non-prod host (TBD — vast.ai or gamma).

---

## 5. INVARIANTS — MUST NOT break (consensus-critical)

1. **The block hash is consensus-defining.** Do NOT change `SigilBlockHeaderV0::hash()`'s output bytes. You may
   CACHE it (compute once, store, memcmp) but the canonical definition stays identical or the node forks itself.
2. **Trusted/zero-get bulk commit is ONLY for the fold-proof + SQIsign-anchor-verified prefix.** Never bulk-trust
   unverified or frontier-tip data. The verified watermark gates it; the frontier window stays full-verify, fail-loud.
3. **Kill-9 crash-safety preserved.** Keep the 2-phase durable commit (blocks durable → tip 'S' → tip durable) and
   torn-tail truncation. SST ingest must be atomic (ingest-or-nothing) with the tip advanced only after.
4. **Windows WAL hazard:** never `append(true)` a file you must `set_len`/truncate (FILE_WRITE_DATA stripped). Use write+seek.
5. **DARK-by-default.** Every new path is behind an env flag, default = current behavior. Divergence=0 vs the
   old path is a hard gate before any default flip.
6. **Fail-loud.** Any divergence/mismatch aborts the fast path and falls back to the checked crawl (zero regression).

---

## 6. ACCEPTANCE CRITERIA & BENCHMARK RIG (L4 owns)

- **Primary:** sustained **≥92,600 blk/s** durable commit on the verified body prefix in the bench rig
  (genesis→tip, divergence=0). Stretch: disk-bound.
- **Checked frontier path:** **≥20,000 blk/s** (the `decode_verify_bench.rs:3,121` master bar) with full verify.
- **Crash test:** `kill -9` mid-commit → reopen → no torn/duplicate/missing block; tip never ahead of durable bodies.
- **Correctness:** byte-identical store contents vs the old checked path over the full range (divergence=0).
- **No regression with flags unset:** behavior + numbers identical to today.
- **Rig:** extend `crates/sigil-state/tests/decode_verify_bench.rs` + add a flux-db commit-throughput bench;
  use the virtual-time / replay rig (do NOT thrash a live node). Report per-stage blk/s (decode | verify | commit).

---

## 7. READY-TO-PASTE PROMPTS

> Each session: first `cat docs/THROUGHPUT_MASTER.md`, then work ONLY your lane, claim your files, coordinate seams on the bus, commit early.

**— SESSION 1 (L1 flux-db bulk engine):**
"Read docs/THROUGHPUT_MASTER.md. You own LANE 1. In flux/crates/flux-db, design + implement (behind `SIGIL_DB_SST_INGEST`, default off): an off-thread sorted-SST builder + an atomic `ingest_sorted_bodies()` that installs a verified body prefix skipping WAL+memtable+compaction; plus a `bulk_mode()` that fully defers compaction during catch-up with a manual compact-at-tip and a write-stall/backpressure guard. Keep durability atomic (ingest-or-nothing, tip advances after). Expose the API for LANE 2. Cite lib.rs:1167-1170, 1303; skeleton.rs as the flat-engine reference. Commit early; broadcast the API surface before landing."

**— SESSION 2 (L2 commit path):**
"Read docs/THROUGHPUT_MASTER.md. You own LANE 2 (crates/sigil-top/src/block_store.rs + block_sync/commit.rs). Make the trusted zero-get path (`put_blocks_bulk_trusted`, :674) the DEFAULT for the fold/anchor-verified prefix (gate on the verified watermark; keep checked path for the frontier). Add an in-memory parent-hash cache so the checked path drops its per-block `height_index_conflict` get (:605). Wire LANE 1's `ingest_sorted_bodies()` into `commit_bulk_trusted_durable`. Preserve the 2-phase kill-9 durability. All behind flags, DARK by default. Coordinate the StoredBlock cached-hash field with LANE 3."

**— SESSION 3 (L3 codec & hashing):**
"Read docs/THROUGHPUT_MASTER.md. You own LANE 3. Collapse the 3 bincode round-trips/record (fetch.rs:436-440) to one pass through decode→verify→commit. Add a cached hash field to StoredBlock so `SigilBlockHeaderV0::hash()` (JSON, ~15-25 µs, block_store.rs:451-456) is computed ONCE and never recomputed — DO NOT change the hash's output bytes (consensus-critical, §5.1). Coordinate the struct change with LANE 2. Prove byte-identical hashes vs current."

**— SESSION 4 (L4 pipeline & bench):**
"Read docs/THROUGHPUT_MASTER.md. You own LANE 4. In block_sync/mod.rs::launch(), turn the drain into a bounded-channel staged pipeline (decode → rayon verify → IO-only commit thread) with backpressure, behind `SIGIL_COMMIT_PIPELINE`. AND build the acceptance rig: extend sigil-state/tests/decode_verify_bench.rs + add a flux-db commit-throughput bench reporting per-stage blk/s, on the virtual-time/replay rig (no live node). Targets §6: ≥92.6k commit, ≥20k checked, divergence=0, kill-9 safe. Your bench is the gate the others measure against — ship it first."

**— CODEX (integrator/implementer of record):**
"Read docs/THROUGHPUT_MASTER.md end-to-end. You are the implementer of record. Take the 4 lanes' designs/patches and LAND the code: implement/merge L1-L4, resolve the seams (L1 ingest API → L2 commit; L2/L3 shared StoredBlock cached-hash field; L4 owns launch() drain), keep it compiling green via flux-cargo-wrapper on the build host (ionice/nice, /home/storage redirects, NOT the prod box), wire every new path behind its env flag DARK-by-default, then run LANE 4's bench and report per-stage blk/s vs the §6 targets. Honor every invariant in §5 — especially: no consensus-hash change, trusted-bulk only for the verified prefix, kill-9 crash-safety, divergence=0 before any default flip. Commit in scoped batches; do not push without sign-off."

---

## 8. FILE INDEX (quick ref)
- `flux/crates/flux-db/src/lib.rs` — LSM core, WAL, flush/compaction, tunables
- `flux/crates/flux-db/src/skeleton.rs` — flat 72-B append engine (~10M/s reference)
- `crates/sigil-top/src/block_store.rs` — chain block store (put_block / batch / bulk_trusted)
- `crates/sigil-top/src/block_sync/commit.rs` — CommitBuffer ring (group-commit)
- `crates/sigil-top/src/block_sync/mod.rs` — launch() sync loop (drain driver)
- `crates/sigil-top/src/block_sync/{fetch.rs,verify.rs,skel_flux.rs}` — pull / verify / skeleton adapter
- `crates/sigil-state/tests/decode_verify_bench.rs` — decode+verify microbench (acceptance rig base)