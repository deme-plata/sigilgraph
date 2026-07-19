# SHARDED-BLOCKSTORE — adoption plan for flux-db ShardedDb (NOT yet wired, on purpose)

## Why this is a PLAN and not a dark flag

RULE 0 (the 5-day sync-rate stall) and this skill's own postmortem name the exact
anti-pattern: `SIGIL_DB_SST_INGEST` and `SIGIL_COMMIT_TRUSTED_PREFIX` shipped DARK
behind flags, fell back silently, and counted as "done" while the live number never
moved. The live sync bottleneck is **the producer's ≤1-per-120ms full-block serve
throttle and the serial point-to-point backfill fetch** — not storage commit. The
local pipeline benches 251k–2.5M blk/s; commit is not where blocks die today.

So BlockStore does NOT get a `SIGIL_DB_SHARDS` flag today. It gets this plan, with
an explicit trigger.

## The trigger (when to wire it)

Wire ShardedDb into BlockStore when BOTH hold:
1. The serve throttle is lifted and the windowed multi-substream fetch has landed
   (block_sync/fetch.rs stub filled) — i.e. blocks arrive faster than one range at
   a time.
2. A LIVE end-to-end sync run (real deployed binaries, real network) measures the
   commit path as the bottleneck — profile shows the sync loop waiting on
   `commit_batch_durable` / `batch_put`, not on the wire.

Until then, sharded commit is capacity the wire cannot feed.

## What the measurement already says (2026-07-18, idle md0)

The flux-db side is proven and waiting (`flux/crates/flux-db/src/shard.rs`,
commit 03812960): 8 KiB entries, put_many(256), presence+content audits 100.00%:

| shards | MB/s |
|---|---|
| 1 | 171 |
| 4 | 441 |
| 8 | 496 (raw dd ceiling: 646) |

The chronos harness IS wired (`CHRONOS_SHARDS`, chronos_scale + chronos_verify,
auto-detects the SHARDS marker) — that is the live consumer proving the engine.

## The seam (what the wiring actually is, measured against block_store.rs today)

`BlockStore.db: flux_db::Database` is the single handle. Call-site inventory:

- **Point ops (shard-clean):** 14 `db.get` + the `db.put` meta/index writes.
  Keys are exact (`blk/<height>`, height-index, `KEY_META` singletons) — FNV
  routing serves them as-is. Note: META keys all land on ONE shard; fine (tiny),
  but the durability barrier must remain `sync_wal()` across ALL shards before
  any watermark (`synced_to`/`verified_to`/marker) advances — ShardedDb's
  all-shard `sync_wal` already has that contract.
- **Bulk commit (the win):** `commit_batch_durable` / `commit_bulk_trusted_durable`
  build `Vec<(&[u8],&[u8])>` batches → `put_many` partitions per shard and writes
  N WAL pipelines in parallel. This is where 171→496 MB/s cashes in.
- **Full scan (the blocker):** `block_store.rs:274` rebuilds indexes at open via
  `db.iter()` (owned snapshot, order-independent consumption). ShardedDb has NO
  iterator yet. Prereq: `ShardedDb::iter_unordered()` — chained per-shard
  snapshots, named so nobody mistakes it for globally sorted. (If any future
  caller needs sorted iteration, that's a k-way merge across shards — do not
  fake it with concatenation.)
- **Migration:** an existing single-store dir has no `SHARDS` marker; ShardedDb
  refuses count mismatches but a single→sharded migration is a rewrite
  (`iter() → put_many` into the new root), offline, with a presence audit before
  the old dir is retired. No in-place conversion.

Wiring shape when triggered: `enum Store { One(Database), Many(ShardedDb) }`
inside BlockStore (same facade chronos_scale uses), constructor takes the choice
from CONFIG (not a hidden env), and the changeover ships only WITH the fetch
pipeline work so it runs live immediately — no dark period.

## Checklist for the wiring PR (when triggered)

- [ ] `ShardedDb::iter_unordered()` in flux-db, with a test asserting full
      coverage across shards
- [ ] `Store` facade in block_store.rs; all 14 gets + meta puts + both bulk
      paths through it
- [ ] all-shard `sync_wal` before EVERY watermark/marker advance (audit each)
- [ ] offline migration tool + audit (old store → sharded root)
- [ ] kill -9 chaos rerun on the sharded store (the v0.36 bar: 100.00% presence)
- [ ] LIVE re-measure: deployed sync rate before/after — the only number that
      counts as done (RULE 0)
