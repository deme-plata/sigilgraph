# SIGIL Fast-Sync Throughput — MASTER SPEC
## Break the flux-db durable-commit wall (~4k → ≥92.6k blk/s)

**Audience:** ChatGPT Codex (implementer of record) + 4 collaborating terminal sessions (lane owners).
**Repo:** `/home/storage/deepseek-codewhale/sigil` · branch `feat/commons-tithe-routing-0.36.1`
**flux-db (path dep):** `/home/storage/deepseek-codewhale/flux/crates/flux-db` (sibling repo)
**Date:** 2026-06-23

---

## ⚡ QUICK START — EXACT PATHS, DO NOT SEARCH

**THIS DOC (absolute):** `/home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md`
Open it directly. Do NOT `grep`/`find` for it.

**STEP 0 — EVERY session loads flux dev skills + MCP combos FIRST:**
- Claude Code: `ToolSearch` → `select:mcp__flux__flux_dev,mcp__flux__flux_sigil_dev,mcp__flux__flux_combo,mcp__flux__flux_file_claim,mcp__flux__flux_file_list,mcp__flux__flux_swarm_message`
- Use `flux_sigil_dev` (check+test+chronos+audit) for your lane; `flux_combo` for quick compile+test.
- Codex / other harness: load the flux MCP dev tools (`flux_dev`, `flux_sigil_dev`, `flux_combo`).

**EXACT FILES PER LANE (absolute — claim these, do not search):**
- **L1 flux-db:** `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/lib.rs` · `…/flux-db/src/cf.rs` · `…/flux-db/src/block.rs` · `…/flux-db/src/skeleton.rs`
- **L2 commit:** `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_store.rs` · `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/commit.rs`
- **L3 codec:** `…/sigil-top/src/block_store.rs` (codec only) · `…/sigil-top/src/block_sync/fetch.rs` · `…/sigil-top/src/block_sync/verify.rs` · `/home/storage/deepseek-codewhale/sigil/crates/sigil-header/src/lib.rs`
- **L4 pipeline+bench:** `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/mod.rs` · `/home/storage/deepseek-codewhale/sigil/crates/sigil-state/tests/decode_verify_bench.rs`

**Build host:** designated non-prod host (NOT epsilon/beta). Build via `flux-cargo-wrapper`, `ionice -c3 nice -n 19`, `TMPDIR`/`CARGO_HOME`/`target` → `/home/storage`.

---

## 0. MISSION

The light-client **skeleton** fast-sync already works: a flat 72-byte append store
(`flux_db::skeleton::SkeletonStore`) bulk-imports the verified prefix at ~10M rec/s. NOT the problem.

The problem is **full block BODIES**: each verified frontier/gossip block is persisted to flux-db's
**custom LSM key-value store** (NOT RocksDB) via the `CommitBuffer` ring. That durable KV path caps at
**~3,863 records/s** (`flux/crates/flux-db/src/skeleton.rs:9`) — the "5k cap". End-to-end sync is
runtime-proven at **39,112 blk/s** vs the chronos transport ceiling **92,600 blk/s** (`commit.rs:41,63`).

**GOAL:** lift the durable full-body commit path from ~4k to **≥92,600 blk/s sustained**, genesis→tip,
**divergence = 0**, **kill-9 crash-safety preserved**, **no consensus block-hash change**, verification
not weakened. Everything ships **DARK-by-default behind env flags**.

---

## 1. GROUND TRUTH (cited)

- **flux-db is a custom LSM, not RocksDB.** Single default CF; WAL → memtable → leveled SST, LZ4. Tunables = constants/setters in `flux-db/src/lib.rs`.
- **Block store** `sigil-top/src/block_store.rs`: `put_block` (`:464`, 2 puts + 2 read-before-write gets); `put_blocks_batch` (`:547`, one batch_put but a per-block get `:605`, "~3.3k" `:664`); `put_blocks_bulk_trusted` (`:674`, zero gets, default OFF `commit.rs:102`).
- **Durable sink** `block_sync/commit.rs::CommitBuffer` (`:111`, batch 16384, fsync once/batch, 2-phase `block_store.rs:813-820`), driven by `block_sync/mod.rs::launch()` (ring `:651`, feed `:898`, drain `:985`, bulk-load `:989-990`).
- **Skeleton prefix DONE:** flat `flux_db::skeleton::SkeletonStore`; commit `98f0449` consolidated onto the flat engine.
- **Serialization:** bodies bincode (`block_store.rs:496…`); `SigilBlockHeaderV0::hash()` JSON-serializes the ~1 KB header (~15-25 µs, `block_store.rs:451-456`).

---

## 2. BOTTLENECK RANKING

**#1** Synchronous leveled-compaction write-amp at the 64 MB WAL flush — real >20k wall (`lib.rs:1167-1170`). Partially mitigated, not eliminated.
**#2** Per-block read-before-write gets (`block_store.rs:664-665`, ~3.3k). Trusted zero-get path exists (`:674`) but OFF; no parent-hash cache for the checked path.
**#3** No bulk SST ingest for bodies; no cross-CF atomic WAL; per-put userspace flush (`lib.rs:1303`). Biggest unbuilt win.
**#4** Redundant bincode round-trips + JSON `header.hash()` (`fetch.rs:436-440`, `block_store.rs:451-456`).

---

## 3. LEVERS (prioritized)

| Pri | Lever | Where |
|----|-------|-------|
| P0 | SST-ingest the verified body prefix (off-thread sorted SSTs, atomic ingest, skip WAL+memtable+compaction) | flux-db + block_store |
| P0 | Trusted zero-get path ON for fold/anchor-verified prefix + parent-hash cache | block_store + commit |
| P1 | Bulk-load compaction discipline (full defer + manual compact-at-tip + write-stall guard) | flux-db |
| P1 | Group-commit WAL (one fsync/syscall per batch) + optional header/body CF split | flux-db |
| P2 | Codec collapse 3→1 + cache `StoredBlock.hash` (compute once) | block_store + fetch/verify |
| P2 | Staged pipeline decode→verify(rayon)→IO-only commit + backpressure | block_sync/mod.rs |

Flags (extend `SIGIL_COMMIT_*`): `SIGIL_DB_SST_INGEST`, `SIGIL_COMMIT_TRUSTED`, `SIGIL_DB_DEFER_COMPACT`, `SIGIL_COMMIT_PIPELINE` — all default = today.

---

## 4. WORK DECOMPOSITION

| Lane | Owner | Scope | Seam |
|------|-------|-------|------|
| **L1 flux-db bulk engine** | Session 1 | off-thread SST build + atomic `ingest_sorted_bodies()`; full compaction-defer + compact-at-tip + write-stall guard | exposes ingest API for L2 |
| **L2 commit path** | Session 2 | trusted zero-get DEFAULT for verified prefix; parent-hash cache; wire L1 ingest | consumes L1 API; hook called by L4 |
| **L3 codec & hashing** | Session 3 | 3→1 bincode; cache `StoredBlock.hash` (NO hash-def change) | shares StoredBlock with L2 |
| **L4 pipeline & bench** | Session 4 | bounded-channel pipeline; blk/s bench + virtual-time rig (acceptance gate) | owns launch() drain |
| **INTEGRATE** | **Codex** | land all lanes, resolve seams, green build, wire flags DARK, run bench, report blk/s | owns merge order |

**Coordination:** claim files via `flux_file_claim`; broadcast seam/API changes via `flux_swarm_message` before landing; snapshot + commit early/often. `block_sync/mod.rs` is hot — L4 owns the drain; L2 adds only a call-site.

---

## 5. INVARIANTS — MUST NOT break

1. **Consensus block hash immutable** — cache it, never change its bytes.
2. **Trusted/zero-get bulk commit ONLY for the fold-proof + SQIsign-anchor-verified prefix.** Frontier stays full-verify, fail-loud.
3. **Kill-9 crash-safety preserved** (2-phase durable, torn-tail truncate; SST ingest atomic, tip advances after).
4. **Windows WAL hazard:** never `append(true)` a file you must `set_len` — write+seek.
5. **DARK-by-default**; divergence=0 vs old path before any default flip.
6. **Fail-loud** — any mismatch aborts the fast path → checked crawl.

---

## 6. ACCEPTANCE & BENCH (L4 owns)

- ≥**92,600 blk/s** durable commit on the verified body prefix, divergence=0.
- Checked frontier path ≥**20,000 blk/s** full-verify (`decode_verify_bench.rs:3,121`).
- `kill -9` mid-commit → reopen → no torn/dupe/missing; tip never ahead of durable bodies.
- No regression with flags unset. Rig: extend `sigil-state/tests/decode_verify_bench.rs` + flux-db commit bench; virtual-time/replay (no live node); per-stage blk/s.

---

## 7. READY-TO-PASTE PROMPTS (paths inlined)

**— SESSION 1 (L1 flux-db bulk engine):**
"STEP 0: load flux dev skills + MCP combos (Claude Code ToolSearch select:mcp__flux__flux_dev,mcp__flux__flux_sigil_dev,mcp__flux__flux_combo,mcp__flux__flux_file_claim). Read /home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md. You own LANE 1. Files: /home/storage/deepseek-codewhale/flux/crates/flux-db/src/{lib.rs,cf.rs,block.rs,skeleton.rs}. Behind SIGIL_DB_SST_INGEST (default off): off-thread sorted-SST builder + atomic ingest_sorted_bodies() that installs a verified body prefix skipping WAL+memtable+compaction; plus bulk_mode() that fully defers compaction during catch-up with manual compact-at-tip + write-stall guard. Atomic durability (ingest-or-nothing, tip advances after). Expose the API for LANE 2. Refs lib.rs:1167-1170,1303; skeleton.rs. flux_file_claim your files; flux_swarm_message the API before landing; commit early."

**— SESSION 2 (L2 commit path):**
"STEP 0: load flux dev skills + MCP combos (as Session 1). Read /home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md. You own LANE 2. Files: /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_store.rs and /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/commit.rs. Make put_blocks_bulk_trusted (block_store.rs:674) DEFAULT for the fold/anchor-verified prefix (gate on verified watermark; keep checked path for the frontier). Add an in-memory parent-hash cache so the checked path drops its per-block height_index_conflict get (block_store.rs:605). Wire LANE 1 ingest_sorted_bodies() into commit_bulk_trusted_durable. Preserve 2-phase kill-9 durability. Flags, DARK default. Coordinate the StoredBlock cached-hash field with LANE 3. Claim files first."

**— SESSION 3 (L3 codec & hashing):**
"STEP 0: load flux dev skills + MCP combos (as Session 1). Read /home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md. You own LANE 3. Files: /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_store.rs (codec only), /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/fetch.rs, /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/verify.rs, /home/storage/deepseek-codewhale/sigil/crates/sigil-header/src/lib.rs. Collapse the 3 bincode round-trips/record (fetch.rs:436-440) to one pass decode→verify→commit. Add a cached hash field to StoredBlock so SigilBlockHeaderV0::hash() (JSON, block_store.rs:451-456) is computed ONCE — DO NOT change the hash output bytes (consensus-critical). Coordinate the struct change with LANE 2. Prove byte-identical hashes. Claim files first."

**— SESSION 4 (L4 pipeline & bench):**
"STEP 0: load flux dev skills + MCP combos (as Session 1). Read /home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md. You own LANE 4. Files: /home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/mod.rs (launch loop) and /home/storage/deepseek-codewhale/sigil/crates/sigil-state/tests/decode_verify_bench.rs. In launch(), make the drain a bounded-channel staged pipeline (decode → rayon verify → IO-only commit thread) with backpressure, behind SIGIL_COMMIT_PIPELINE. Build the acceptance rig: extend decode_verify_bench.rs + add a flux-db commit-throughput bench reporting per-stage blk/s on the virtual-time/replay rig (no live node). Targets: ≥92.6k commit, ≥20k checked, divergence=0, kill-9 safe. Ship the bench FIRST — others measure against it. Claim files first."

**— CODEX (integrator/implementer of record):**
"STEP 0: load the flux MCP dev tools (flux_dev, flux_sigil_dev, flux_combo). Read /home/storage/deepseek-codewhale/sigil/docs/THROUGHPUT_MASTER.md end-to-end. You are the implementer of record. Land L1-L4 (files per lane in §7 + QUICK START), resolve seams (L1 ingest API→L2; L2/L3 shared StoredBlock cached-hash; L4 owns launch() drain), keep green via flux-cargo-wrapper on the build host (ionice/nice, /home/storage redirects, NOT prod), wire every new path behind its env flag DARK-by-default, run LANE 4 bench, report per-stage blk/s vs §6. Honor all §5 invariants — no consensus-hash change, trusted-bulk only for verified prefix, kill-9 safety, divergence=0 before any default flip. Commit in scoped batches; no push without sign-off."

---

## 8. FILE INDEX (absolute)
- `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/lib.rs` — LSM core, WAL, flush/compaction
- `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/cf.rs` — CF / Database engine
- `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/block.rs` — SST builder/reader
- `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/skeleton.rs` — flat 72-B append engine
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_store.rs` — chain block store
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/commit.rs` — CommitBuffer ring
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/mod.rs` — launch() sync loop
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/fetch.rs` — pull / codec
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-top/src/block_sync/verify.rs` — verify / fast-forward
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-header/src/lib.rs` — header + hash()
- `/home/storage/deepseek-codewhale/sigil/crates/sigil-state/tests/decode_verify_bench.rs` — bench base