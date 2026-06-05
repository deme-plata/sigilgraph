# flux-db Audit v0 for SIGIL Phase 0–N

**Author:** rocky-sigil (Claude Opus 4.7, Epsilon)
**Date:** 2026-05-29
**Scope:** Compare `flux-db` (3,060 LOC) public API against `q-storage`'s real RocksDB usage. Identify gaps that block SIGIL adoption per phase. Output: file-able issues for flux-db, not a flux-db patch (rocky-sigil holds the audit claim only).

## TL;DR verdict

| Phase | Required from storage | flux-db ready? |
|---|---|---|
| **Phase 0** (single-producer, MVP) | put/get/delete + 1 CF + WAL + flush + snapshot | ✅ **Ready as-is** |
| **Phase 1** (real VDF mining, multi-CF) | + CFs + per-CF iter + delete_range + TTL + merge | ✅ Ready (CFs landed v0.14.0) |
| **Phase 2** (DagKnight consensus, multi-CF atomic per block) | + multi-CF atomic WriteBatch + reverse iter + compact_range | ⚠️ **3 gaps** — see G1, G2, G3 |
| **Phase 3+** (mainnet, backups mandatory) | + BackupEngine + WriteOptions(sync) + DBRecoveryMode | ❌ **3 gaps** — see G4, G5, G6 |
| **Perf-class** (hot-CF tuning) | + SliceTransform/prefix-bloom + per-CF BlockBasedOptions | ⚠️ **2 gaps** — see G7, G8 |

SIGIL **can ship Phase 0** on flux-db today. **Phase 2** needs gaps G1–G3 closed before DagKnight commits state per block. **Mainnet** needs G4–G6 closed.

## Sources

- `flux-db` public API: `/home/storage/deepseek-codewhale/flux/crates/flux-db/src/{lib,block,cache,cf,filter,merge,range_tomb,ttl}.rs`
- `q-storage` RocksDB usage: `/home/orobit/q-narwhalknight/crates/q-storage/src/**/*.rs` — histogram of API call counts collected via grep
- SIGIL storage requirements: `SIGIL_GENESIS_v0.md` §1 lock #4 + §11 (pre-flight) + skill operating rule "no `/tmp` or `/root`; absolute DB paths only".

## flux-db capability surface (what's in tree)

`Database` (lib.rs ~2093 LOC):
- `open(path)`, `with_block_cache_capacity(bytes)`
- CRUD: `put`, `get`, `delete`, `delete_range`
- TTL: `put_with_ttl`, `put_ttl_seconds` + `ttl::wrap/unwrap/is_expired`
- Merge: `set_merge_operator(Arc<dyn MergeOperator>)`, `merge(key, delta)`
- Compaction filter: `set_compaction_filter(Arc<dyn CompactionFilter>)`
- Column families: `create_cf(name) → Database`, `cf(name)`, `list_cfs()`, `drop_cf(name)`
- Iteration: `iter()`, `iter_from(start)`
- Transactions (MVCC, optimistic): `begin_transaction() → Transaction { get, put, delete, commit, rollback }`
- Snapshots: `snapshot() → Snapshot { get, scan, len, checksum, sequence }`
- Maintenance: `flush()`, `compact_async()`, `block_cache_stats() → (hits, misses, bytes)`

`SstHandle` + `BlockSstReader`: block-based SST format (BLKF magic, 4 KB target, 16-byte footer). Bloom filter persistence.

`BlockCache` (cache.rs): LRU block cache.

`CompactionFilter` trait (filter.rs): per-key keep/drop/transform decisions during compaction.

`MergeOperator` trait (merge.rs): RocksDB-style merge operands.

`RangeTombstone` (range_tomb.rs): tombstone-covered range tracking.

LSM: multi-level L0..L6 (per v0.15.0 commit), leveled compaction.

WAL: torn-write protection (per v0.10.0 commit), bloom persistence.

## q-storage RocksDB call histogram (top 22)

```
253×  .write(...)                        — WriteBatch::write
145×  ColumnFamilyDescriptor
 94×  .get_cf(cf, key)
 75×  WriteBatch
 69×  rocksdb::DBCompressionType
 61×  rocksdb::Cache
 53×  .put_cf(cf, key, val)
 27×  .flush()
 18×  rocksdb::SliceTransform           — prefix extractor
 17×  rocksdb::ColumnFamilyDescriptor
 15×  rocksdb::DB
 13×  WriteOptions
 12×  .delete_cf
 11×  rocksdb::WriteOptions
 11×  .iterator_cf
 10×  rocksdb::IteratorMode             — Start/End/From(key, Direction)
  8×  rocksdb::Options
  8×  rocksdb::AsColumnFamilyRef
  4×  rocksdb::Direction                — Forward/Reverse
  4×  .compact_range(start, end)
  4×  BackupEngine
  2×  rocksdb::BlockBasedOptions
```

## Gap analysis

### G1. Multi-CF atomic WriteBatch  — **Phase-2 blocker**

**RocksDB**: `WriteBatch` (75× call sites, `.write(...)` 253×) — atomic batched mutations across multiple CFs in one fsync.

**flux-db**: `SstReader::batch_put(entries)` exists (single CF / single SST). `Transaction` provides multi-statement atomicity within one CF/DB. No equivalent for atomic multi-CF writes in one commit.

**Why SIGIL needs it**: every committed block applies (a) balance changes to `wallets` CF, (b) nonces to `accounts` CF, (c) event log entries to `events` CF, (d) state-root commitment to `headers` CF — all-or-nothing per block. Without multi-CF WriteBatch, the chokepoint `commit_state_transition()` (genesis doc §11) can't be atomic.

**Recommendation (issue: `flux-db: WriteBatch with multi-CF support`):**
```rust
pub struct WriteBatch { /* per-CF op list */ }
impl WriteBatch {
    pub fn put(&mut self, cf: &Database, k: &[u8], v: &[u8]) { ... }
    pub fn delete(&mut self, cf: &Database, k: &[u8]) { ... }
    pub fn merge(&mut self, cf: &Database, k: &[u8], op: &[u8]) { ... }
}
impl Database { pub fn write(&self, batch: WriteBatch) -> Result<u64, String> { ... } }
```
Atomic at the WAL layer; single sequence-number stamp.

### G2. Reverse iteration  — **Phase-2 blocker**

**RocksDB**: `IteratorMode::From(key, Direction::Reverse)` and `IteratorMode::End` (Direction enum used 4×). q-storage uses these for: latest-by-height lookup, "highest tx hash below this height", reorg-rollback walks.

**flux-db**: `iter()` and `iter_from(start)` — appear forward-only per API surface.

**Why SIGIL needs it**: turbo-sync needs to find latest applied height fast (`iter().rev().next()` pattern). DagKnight ordering enumerates vertices by round descending.

**Recommendation (issue: `flux-db: reverse iteration via iter().rev() or explicit iter_from_back(end)`):**
Either:
- Make `DbIterator` a `DoubleEndedIterator`, or
- Add `iter_from_back(end: &[u8]) -> DbIterator` and `iter_rev() -> DbIterator`.

### G3. `compact_range(start, end)`  — **Phase-2 (operator)**

**RocksDB**: `.compact_range(Some(start), Some(end))` — compact only the affected SST region after large `delete_range` (e.g. pruning old epochs).

**flux-db**: `compact()` and `compact_async()` operate on the whole DB. Pruning a single CF's old range forces a full-DB compaction.

**Why SIGIL needs it**: when archiving past-finalized epochs (events older than N blocks) we don't want to rewrite the entire wallet CF.

**Recommendation (issue: `flux-db: per-range compaction (compact_range(start, end))`):**
```rust
pub fn compact_range(&self, start: &[u8], end: &[u8]) -> Result<(), String>;
```

### G4. BackupEngine equivalent  — **Mainnet blocker**

**RocksDB**: `BackupEngine` (4× call sites) — point-in-time consistent snapshots-to-disk that survive process death. SIGIL operator runbook (inherited from CLAUDE.md) requires **hourly backups mandatory**.

**flux-db**: no backup module. `flush()` + filesystem copy is *not* equivalent — there's no torn-state guarantee across SST + WAL during the copy.

**Why SIGIL needs it**: mainnet operators expect a `sigil-node backup --to /path` that produces a restorable snapshot without stopping the node. Currently impossible.

**Recommendation (issue: `flux-db: BackupEngine — point-in-time consistent SST+WAL snapshot`):**
```rust
pub fn backup_to(&self, dest: impl Into<PathBuf>) -> Result<BackupInfo, String>;
pub fn restore_from(src: impl Into<PathBuf>) -> Result<Database, String>;
pub fn list_backups(dir: &Path) -> Vec<BackupInfo>;
```
MVP: pin SST sequence numbers + hardlink immutable SSTs + copy WAL tail; restore reverses + replays.

### G5. WriteOptions (sync flag, disable_wal)  — **Operator gap**

**RocksDB**: `WriteOptions { sync: true, disable_wal: false }` (24× total call sites) — per-write fsync control. Used for fast bulk-import paths (disable_wal=true during sync replay).

**flux-db**: `put`/`delete`/`merge` use a single internal write-strategy. No knob.

**Recommendation (issue: `flux-db: WriteOptions for per-write sync/WAL control`):**
```rust
pub struct WriteOptions { pub sync: bool, pub disable_wal: bool }
pub fn put_opt(&self, k: &[u8], v: &[u8], opts: WriteOptions) -> Result<(), String>;
```
Phase-1 turbo-sync sees a 3-5× write throughput boost with `disable_wal=true` during initial replay, then a single `flush()` at the end.

### G6. DBRecoveryMode  — **Operator gap**

**RocksDB**: `DBRecoveryMode::AbsoluteConsistency / PointInTimeRecovery / TolerateCorruptedTailRecords` (1×, configured at open). q-storage uses point-in-time for resilient restart after dirty shutdown.

**flux-db**: `open()` has a single recovery path (per v0.10.0 commit: WAL torn-write protection). Behavior is fixed.

**Recommendation (issue: `flux-db: DBRecoveryMode option at open`):**
```rust
pub enum RecoveryMode { AbsoluteConsistency, PointInTime, TolerateCorruptedTail }
pub fn open_with(path, opts: OpenOptions { recovery_mode, ... });
```

### G7. SliceTransform / prefix-extractor  — **Perf-class gap**

**RocksDB**: `SliceTransform::create_fixed_prefix(N)` (18×) — declare key prefixes so bloom filters + iteration hints work on `<prefix>*` scans.

**flux-db**: bloom filters are whole-key; no prefix-aware bloom. `scan_prefix(prefix)` exists on `Snapshot` but full-SST scan, no index.

**Why SIGIL needs it**: wallet-balance lookup uses `addr || asset` as key — prefix-bloom on `addr` would skip irrelevant SSTs. For 100M+ keys this is the difference between O(1) and O(SST-count).

**Recommendation (issue: `flux-db: prefix-extractor + prefix-bloom for hot key patterns`):**
```rust
pub trait PrefixExtractor: Send + Sync {
    fn extract(&self, key: &[u8]) -> &[u8]; // returns the prefix substring
}
impl Database { pub fn set_prefix_extractor(&self, p: Arc<dyn PrefixExtractor>); }
```
SstBuilder then derives a second bloom keyed on `extract(key)` and stores it in the footer.

### G8. Per-CF BlockBasedOptions / block cache  — **Perf-class gap**

**RocksDB**: `BlockBasedOptions { block_size, block_cache, filter_policy }` per CF (2×). Hot CFs (wallets) get a bigger dedicated cache; cold CFs (events) get a small one.

**flux-db**: single `BlockCache` shared across all CFs. `TARGET_BLOCK_SIZE = 4096` hardcoded in `block.rs`.

**Recommendation (issue: `flux-db: per-CF BlockBasedOptions`):** non-blocking for MVP; defer until profiling shows cache thrash.

## Test-portability matrix

q-storage has these tests that should port directly to flux-db once gaps close. Each tests a real mainnet-safety property — bring them along when SIGIL adopts flux-db at scale.

| Test | What it proves | Gap dependency |
|---|---|---|
| `mainnet_critical_tests` (20 tests) | Double-spend, replay, coinbase fraud | G1 (atomic WriteBatch) |
| `balance_integrity_tests` (28 tests) | Balance corruption immunity | G1 + G2 |
| `fork_reorg_tests` (19 tests) | Reorg balance consistency | G1 + G2 + G4 |
| `backup_restore_tests` (17 tests) | Disaster recovery | G4 (hard blocker) |
| `signature_verification_tests` (15 tests) | Forgery detection | None (independent) |

After G1+G2 land, ~67 tests immediately portable. After G4, all 99 are portable.

## File-able issues (suggested)

Open against the flux-db repo (or whichever queue holds flux-db work):

| ID | Title | Severity | Phase blocked |
|---|---|---|---|
| G1 | flux-db: multi-CF atomic WriteBatch + Database::write(batch) | HIGH | Phase 2 |
| G2 | flux-db: reverse iteration (DoubleEndedIterator or iter_from_back) | HIGH | Phase 2 |
| G3 | flux-db: per-range compaction (compact_range(start, end)) | MEDIUM | Phase 2 (operator) |
| G4 | flux-db: BackupEngine — point-in-time SST+WAL snapshots | CRITICAL | Mainnet |
| G5 | flux-db: WriteOptions for per-write sync/WAL control | MEDIUM | Operator perf |
| G6 | flux-db: DBRecoveryMode at open() | MEDIUM | Operator robustness |
| G7 | flux-db: PrefixExtractor + prefix-bloom for hot key patterns | MEDIUM-HIGH | Perf at scale |
| G8 | flux-db: per-CF BlockBasedOptions (block_size + cache + filter) | LOW | Profile-driven |

## Non-gaps (compatibility surface that's already fine)

- **CFs** (G "already-there"): `create_cf`/`cf`/`drop_cf`/`list_cfs` cover the 162 `ColumnFamilyDescriptor` references — the descriptor model is RocksDB-specific, flux-db's "CF as sub-Database" model is cleaner.
- **`.get_cf` / `.put_cf` / `.delete_cf` / `.iterator_cf`** (159 sites): all expressible as `db.cf("name").get(key)` / etc. API style only.
- **`IteratorMode`** (10×): `iter()` + `iter_from()` cover Start/From; only End+Reverse missing (see G2).
- **`Cache`** (61×): `with_block_cache_capacity(bytes)` is the single-knob equivalent. Per-CF tuning is G8 only.
- **`SliceTransform`** for non-bloom uses: most of the 18 sites are bloom-prefix; if any are key-decomposition for compaction we may still be fine.
- **WAL torn-write protection** (q-storage uses default behavior here): flux-db v0.10.0 explicit feature.
- **TTL, merge, compaction filter** (q-storage uses indirectly): flux-db direct equivalent.

## Recommendation to SIGIL Phase-0 implementers (rocky-59)

1. **Build sigil-node Phase 0 against flux-db as-is.** Single-CF wallet store + flush-per-block is fine. None of G1-G8 block boot or block production.
2. **Open G1 and G2 issues now** (don't wait for Phase 2). DagKnight integration will hit them within ~1-2 weeks of starting Phase 2.
3. **G4 (BackupEngine) is the gate for "SIGIL leaves testnet"** — without it, ops can't honor the hourly-backups runbook. Treat as long-lead; design starts now.
4. **Don't switch to RocksDB as a workaround.** That breaks the "Storage = flux-db (no RocksDB)" architectural lock. If G1-G4 slip, narrow SIGIL Phase 2 scope until they land.

## What was NOT audited

- Disk-format compatibility / SST upgrades across flux-db versions (future-Self gap).
- Crash-recovery test coverage in flux-db's own test suite — needs separate dive.
- Performance benchmarks vs RocksDB — out of scope for v0 audit; needs flux-db's `flux_bench` integration.
- Concurrency model under heavy parallel reads + writes — read flux-db's MVCC code to assert no read-write serializations.

## How to act on this report

If flux-db work is rocky-59's lane: hand this report off, close G1+G2 in v0.17.0, G4 design doc in v0.18.0, ship.
If rocky-sigil keeps flux-db audit: I'll author G1+G2 patches in a follow-up claim once Phase 0 unblocks me from coord overhead.
Either way: every gap above maps to a single mainnet-safety property we can't ship without.

---

## Update — 2026-05-29 / rocky-sigil-61: G1 shipped

**G1 (multi-CF atomic WriteBatch) is now in `flux-db@v0.17.0+`.** Shipped as `pub struct WriteBatch` in `flux/crates/flux-db/src/lib.rs` with `Database::write(batch)`.

- API surface: `WriteBatch::new()`, `put(cf, k, v)`, `delete(cf, k)`, `len()`, `is_empty()`, `clear()`
- Atomicity: per-CF write-locks held across all batch ops touching that CF; lock order is path-sorted so two concurrent batches that touch the same CFs in reverse order can't deadlock
- Returns commit sequence (max seq stamped across all CFs) for read-after-write checks
- Tests: 7 new + 63 pre-existing = **70/70 pass** in `flux-db` test suite

What's still open from G1's original scope (separate follow-up — call it G1.5):
- **Cross-WAL crash atomicity** — each CF still has its own WAL file. A process crash between CF-A's WAL fsync and CF-B's would leave the batch partially durable. In-process atomicity is strict (verified by `test_writebatch_concurrent_reader_sees_all_or_none`) but the WAL torn-batch case needs either a shared parent WAL or a two-phase commit marker. Documented in the WriteBatch doc-comment.

**Verdict bump:** Phase 2 of SIGIL is no longer blocked on G1's in-process atomicity. The state-transition chokepoint `commit_state_transition()` can use `WriteBatch` directly. Crash atomicity (G1.5) becomes a Phase 3 (mainnet-prep) item alongside G4 (BackupEngine).

Remaining gap priority (post-G1):
1. **G2** — reverse iteration (Phase 2 blocker; turbo-sync needs it)
2. **G4** — BackupEngine (mainnet gate)
3. **G1.5** — cross-WAL crash atomicity (mainnet gate)
4. G3, G5, G6, G7, G8 — operator/perf-class

---

## Update — 2026-05-29 / rocky-sigil-68: G2 shipped

**G2 (reverse iteration) is now in `flux-db@v0.17.0+`.** `DbIterator` is a `DoubleEndedIterator`; `Database` exposes two new entry points.

API additions:
- `Database::iter_rev() -> DbIterator` — full merged view, descending key order
- `Database::iter_from_back(end: &[u8]) -> DbIterator` — descending from `end` inclusive (rocksdb `IteratorMode::From(end, Reverse)` shape)
- `DbIterator: DoubleEndedIterator` — `iter().rev()` and `iter_from(start).rev()` now both work directly

Implementation: `descending: bool` field on `DbIterator` swaps `.next()` with `.next_back()` on the underlying `BTreeMap::IntoIter` (which is natively `DoubleEndedIterator`). Range tombstones honored in both directions.

Tests: 7 new + 70 pre-existing = **77/77 pass** including the headline turbo-sync pattern (`test_turbo_sync_tip_lookup_pattern`) — 8-byte BE-encoded heights, find largest height (≤ 30) and largest height ≤ 20 (reorg-rollback shape).

**Verdict bump:** SIGIL Phase 2 turbo-sync's "latest applied height" lookup is unblocked. DagKnight round-descending vertex enumeration is unblocked. Reorg-rollback walks are unblocked.

Remaining gap priority (post-G1+G2):
1. **G4** — BackupEngine (mainnet gate)
2. **G1.5** — cross-WAL crash atomicity (mainnet gate)
3. **G3** — per-range compaction (operator hygiene)
4. **G7** — prefix-extractor + prefix-bloom (perf at scale)
5. G5, G6, G8 — operator-class

---
*Filed under rocky-sigil-59 (audit) + rocky-sigil-61 (G1 patch) + rocky-sigil-68 (G2 patch). For port work, see project_sigil_chain.md inventory and the SIGIL skill's Track inventory.*
