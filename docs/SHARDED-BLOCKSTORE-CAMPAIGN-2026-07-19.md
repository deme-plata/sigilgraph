# SHARDED-BLOCKSTORE campaign run — 2026-07-19 (rocky)

Execution record against `SHARDED-BLOCKSTORE-PLAN.md`. The plan's RULE-0
discipline is honored: **BlockStore is still NOT wired** — the trigger
(serve throttle lifted + a LIVE profile showing commit as the bottleneck) has
not fired, so no `Store` facade and no dark flag shipped. What ran is every
checklist item that is runnable *before* the trigger.

## Checklist status

- [x] `ShardedDb::iter_unordered()` in flux-db, with coverage test —
      **flux dc414b74** (chained per-shard snapshots, tombstone-resolved,
      explicitly unordered; test = exact live-set across 8 shards with
      tombstones + per-shard-count reconciliation). flux-db 127/127 green.
- [ ] `Store` facade in block_store.rs — **PARKED on the trigger, on purpose.**
- [x] all-shard `sync_wal` before watermark advance — contract already held by
      `ShardedDb::sync_wal` (parallel barrier over every shard); exercised by
      the chaos runs below (marker-after-fsync).
- [x] offline migration tool + audit — **`flux_db_shard_migrate`**
      (flux dc414b74): iter → put_many rewrite, all-shard fsync barrier, then
      MANDATORY dual audit (full recount via iter_unordered + evenly-spaced
      byte-for-byte value samples); exit 0 only on both passing. Refuses
      sharded sources and non-empty destinations (both guards test-fired).
- [x] kill -9 chaos rerun on the sharded store — **PASSED at the v0.36 bar**,
      two cycles (below).
- [ ] LIVE re-measure of deployed sync rate — **PARKED on the trigger**; the
      only number that counts as done for the wiring itself.

## Run A — kill -9 chaos (4 shards, chronos_scale/verify harness)

Real writer PID confirmed via /proc/exe before each kill; marker verified
static post-kill (an earlier attempt invalidated itself by killing a wrapper
subshell and orphaning the writer — store discarded, rerun clean).

| cycle | scenario | durable marker | audit (≤ marker) |
|---|---|---|---|
| 1 | fresh 4-shard flood, `kill -9` mid-write | 120,000 | **20,000/20,000 = 100.00%** |
| 2 | resume with `CHRONOS_SHARDS` unset (SHARDS-marker auto-detect, 524fbdd), `kill -9` again | 190,000 | **21,112/21,112 = 100.00%** |

Nothing at-or-below the fsync-covered marker was lost in either cycle;
resume adopted the persisted shard count ("adopting 4 shards from SHARDS
marker").

## Run B — shard-width sweep re-measure (flux_db_shard_bench, 1 GiB, io-niced)

| shards | MB/s (2026-07-19) | plan table (2026-07-18) | audit |
|---|---|---|---|
| 1 | 205 | 171 | 20,000/20,000 100.00% |
| 4 | 504 | 441 | 20,000/20,000 100.00% |
| 8 | 529 | 496 | 20,000/20,000 100.00% |

Shape confirmed: ~2.5× at 4 shards, plateau toward 8 (array ceiling ~646).
Absolute numbers slightly above the 07-18 table (array load differs); the
conclusion is unchanged — sharded commit capacity is proven and waiting on
the wire.

## Run C — migration rehearsal (single → 4 shards)

Built a REAL single-Database chronos store (40,000 × 8 KiB blocks,
TARGET-terminated at 331 MB), then:

- `flux_db_shard_migrate single sharded 4` → 40,000 entries at 184 MB/s
- audit recount: dst=40,000 == src=40,000 — OK
- audit values: 20,000/20,000 byte-identical — OK → "MIGRATION VERIFIED"
- `chronos_verify` against the MIGRATED root: auto-detected 4 shards,
  presence 20,000/20,000 (100.00%)
- both refusal guards fired correctly (sharded src / non-empty dst, exit 2)

## What unblocks the rest

Items 2 and 6 wire + measure together, per the plan: when the serve throttle
is lifted and a live profile shows the sync loop waiting on
`commit_batch_durable`, ship the `Store` facade WITH the fetch pipeline and
re-measure the deployed sync rate. Every prerequisite on the storage side is
now built, audited, and rehearsed.

## Addendum 2026-07-20 — throttle lift DEPLOYED LIVE + RULE-0 measure

The trigger's condition #1 is now LIVE, not just landed: the branch binaries
(serve throttle removed + immutable-range serve-cache + windowed request-ahead
fetch, sigil-node/sigil-top release build of 2026-07-19 HEAD) were deployed to
the sole producer (operator-approved restart 06:41; exact prior bytes kept at
`target/release/sigil-node.bak-live-20260719`). Boot replay of the 30.2M-block
chain took 7.8 min; production resumed with a byte-identical wallet state root;
zero panics.

**LIVE sync measure (fresh follower vs live producer, 3-min sustained):**

| | blk/s |
|---|---|
| old live baseline (June, throttled serve + serial fetch) | 144 |
| local smoke (2026-07-19, small chain, concurrent production) | ~1,360 |
| **LIVE post-lift (2026-07-20, 2.76M blocks applied in 3 min)** | **15,348** |

~107× the old live baseline. The producer minted uninterrupted while serving
the full-rate backfill (serve-cache filling on first cold sweep). Next step
for the Store-facade trigger (condition #2): profile WHERE the follower's
15.3k/s loop waits — wire, verify, or commit — against the 251k–2.5M/s local
commit-pipeline bench. If it waits on commit, the ShardedDb wiring fires.
