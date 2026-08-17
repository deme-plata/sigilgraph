// sigil-top/src/block_store.rs — Local block storage via flux-db (v0.7.1)
//
// Persists SIGIL block headers to flux-db, maintains chain continuity,
// and provides query methods for the dashboard.
//
// v0.7.1: Added streaming aether sync — reads sigil-node shards directly
// into flux-db, one block at a time via serde_json.

use serde::{Deserialize, Serialize};
use sigil_header::{SigilBlockHeaderV0, BlockHash};
use std::path::PathBuf;

/// Live progress for a store that's still opening. WAL replay is the one open-time
/// cost that actually scales with store size (see `flux_db::Database::open_with_progress`
/// — measured ~330 MB/s, so a multi-GB WAL genuinely takes real seconds, not a stall).
/// `total == 0` means "not started yet / no WAL to replay" — render that as indeterminate,
/// not as "0 of 0 bytes done". Shareable: created before the opener thread spawns, polled
/// by the TUI's render loop every frame regardless of keypresses.
#[derive(Default)]
pub struct OpenProgress {
    consumed: std::sync::atomic::AtomicU64,
    total: std::sync::atomic::AtomicU64,
    finished: std::sync::atomic::AtomicBool,
}

impl OpenProgress {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// `(bytes_consumed, total_bytes, finished)`. `finished` flips true once the WAL
    /// replay (if any) completes — a caller showing a gauge should treat `finished`
    /// as authoritative over a stale `consumed == total` read (the store may still be
    /// doing fast, non-progress-reported work like the SST-listing pass after replay).
    pub fn snapshot(&self) -> (u64, u64, bool) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.consumed.load(Relaxed), self.total.load(Relaxed), self.finished.load(Relaxed))
    }

    fn set(&self, consumed: u64, total: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.consumed.store(consumed, Relaxed);
        self.total.store(total, Relaxed);
    }

    /// `pub` (not `pub(crate)`): `open_store_with_fallbacks` (sigil-top's main.rs) calls
    /// this directly on paths that never touch `open_with_timeout_and_progress` at all —
    /// the light-boot/oversized-store shortcut opens a fresh volatile store near-instantly
    /// and would otherwise leave `finished` permanently false, stranding the UI's gauge.
    pub fn mark_finished(&self) {
        self.finished.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredBlock {
    pub header: SigilBlockHeaderV0,
    pub hash_hex: String,
    pub synced_at: u64,
}

pub struct BlockStore {
    db: flux_db::Database,
    best_height: u64,
    best_hash_hex: String,
    /// v0.10.0: GENESIS ANCHOR. The lowest height the chain's backfill actually serves —
    /// SIGIL's genesis (height 0) is minted locally and is NOT served by the range-backfill
    /// endpoint once it's pruned from the producer's RAM window, so `synced_to`/`verified_to`
    /// can never reach it. `base` is the trust anchor: contiguity + verification both start
    /// here, and block at `base` is accepted without a parent-linkage check (we can't fetch
    /// its parent). Default 0 (full chain); set to 1 for SIGIL via `set_base`.
    base: u64,
    /// Contiguous synced count: blocks 0..synced_to are ALL present, so `synced_to`
    /// is the next height needed. This (not `best_height`, which a stray live block
    /// inflates) drives the backfill cursor + the progress bar, so sync walks forward
    /// sequentially and resumes correctly instead of re-walking from 0.
    synced_to: u64,
    /// v0.9.0: Contiguous CRYPTOGRAPHICALLY-VERIFIED count: blocks 0..verified_to have
    /// each passed `precheck()` AND link to their parent (`header[h].parent_hash ==
    /// header[h-1].hash()`). `synced_to` only means "downloaded"; `verified_to` means
    /// "downloaded AND validated as one connected chain". Persisted so a restart resumes
    /// verification instead of re-walking 0. Invariant: `verified_to <= synced_to`.
    verified_to: u64,
    /// LANE-S: the GENESIS-anchor hash the persisted watermarks belong to (the hex hash of the
    /// block at `base`). The watermarks describe ONE chain; a testnet restart mints a fresh
    /// genesis, so a different hash here means the persisted synced_to/verified_to are for a
    /// DEAD chain and must be wiped (see `note_genesis`). Empty until the anchor block is seen.
    genesis_hash: String,
    /// v3 (LANE-C, for LANE-B's fold fast-path): the explicit FOLD ANCHOR — `(height, hash)` of
    /// the checkpoint a fold-proof authorizes (hash = the DNS SQIsign tip hash B authenticates
    /// against). Below this height the prefix is TRUSTED-not-downloaded, so `verified_to` may
    /// legitimately exceed `synced_to` up to `fold_anchor.height`; at/above it `verified_to`
    /// stays ≤ `synced_to` (the frontier IS downloaded + precheck'd). `None` = no fold anchor →
    /// the strict `verified_to ≤ synced_to` clamp (today's behavior). Persisted under meta 'A' so
    /// the clamp-relax survives a reopen (else verified_to would be clamped back down on restart).
    fold_anchor: Option<(u64, BlockHash)>,
    /// THROUGHPUT_MASTER LANE 2: in-memory parent-hash / height-index cache for the checked batch
    /// path. DARK by default (`SIGIL_COMMIT_PARENT_CACHE`). Holds the `hash_hex` of heights THIS
    /// process committed so the checked path can (a) skip the guaranteed-miss `height_index_conflict`
    /// `db.get` for heights strictly above `best_height` and (b) verify the batch-boundary parent
    /// linkage from memory instead of `get_stored_at_height(h-1)`. Always falls back to disk on a
    /// miss, so it can only ever DROP a redundant read — never change an accept/reject outcome.
    link_cache: LinkCache,
    /// THROUGHPUT_MASTER LANE 2: route `commit_bulk_trusted_durable` through flux-db's off-thread
    /// sorted-SST builder + atomic ingest (LANE 1's `ingest_sorted_bodies`) instead of `batch_put`.
    /// DARK by default (`SIGIL_DB_SST_INGEST`). Until the flux-db API lands it falls back to the
    /// `batch_put` bulk path, so the flag is wirable + green without a hard dep on the unlanded seam.
    sst_ingest: bool,
}

/// THROUGHPUT_MASTER LANE 2 — in-memory committed-height → hash_hex cache. Bounded ring (evict the
/// lowest height past `cap`). It is a pure read-elision aid for the checked batch path: every lookup
/// that misses falls through to flux-db, and the only heights it ever serves are ones we durably
/// committed this run (so a hit is byte-for-byte what the disk index holds). Cleared on chain reset.
struct LinkCache {
    enabled: bool,
    map: std::collections::BTreeMap<u64, String>,
    cap: usize,
}

impl LinkCache {
    fn new(enabled: bool) -> Self {
        // ~128k entries ≈ a few large backfill batches of look-back, bounded so a genesis→tip sync
        // never grows it without limit.
        LinkCache { enabled, map: std::collections::BTreeMap::new(), cap: 131_072 }
    }
    fn from_env() -> Self {
        Self::new(
            std::env::var("SIGIL_COMMIT_PARENT_CACHE").ok()
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(false),
        )
    }
    /// Record a durably-committed (height, hash_hex). No-op when disabled.
    fn record(&mut self, height: u64, hash_hex: &str) {
        if !self.enabled { return; }
        self.map.insert(height, hash_hex.to_string());
        while self.map.len() > self.cap {
            // BTreeMap keeps keys sorted → the first key is the lowest height; evict it.
            if let Some(&lo) = self.map.keys().next() { self.map.remove(&lo); } else { break; }
        }
    }
    /// The cached hash_hex for `height`, if we committed it this run. None when disabled.
    fn get(&self, height: u64) -> Option<&String> {
        if self.enabled { self.map.get(&height) } else { None }
    }
    fn clear(&mut self) { self.map.clear(); }
}

/// Key prefix bytes (block data is keyed by 64-char hex hash, never starts with these).
const KEY_HINDEX: u8 = 0x01; // 0x01 ++ height.to_be_bytes() -> hash_hex  (height index)
const KEY_META: u8 = 0x02;   // 0x02'S' -> synced_to · 0x02'V' -> verified_to  (meta)
                             // 0x02'B' -> be(best_height) ++ best_hash_hex  (v0.35: O(1) open)
                             // 0x02'G' -> genesis-anchor hash hex            (LANE-S: reset key)
                             // 0x02'A' -> be(fold_anchor_height) ++ 32B hash (v3 LANE-C: fold anchor)

fn height_key(h: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(KEY_HINDEX);
    k.extend_from_slice(&h.to_be_bytes());
    k
}

fn short_hash_hex(hash: &str) -> &str {
    &hash[..hash.len().min(16)]
}

impl BlockStore {
    /// Open for the interactive TUI: returns IMMEDIATELY. A store that predates the
    /// 'B' fast-open key migrates its index on a BACKGROUND thread (see
    /// [`migrate_index`](Self::migrate_index)) so the dashboard paints at once
    /// instead of hanging on a multi-GB scan. Under `cfg!(test)` migration is inline
    /// so tests observe `best_height` synchronously.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::open_inner(path, false, None)
    }

    /// Open for headless tooling (verify / full-sync / explorer serve) that needs
    /// `best_height` + the height index on return: the migration runs INLINE.
    pub fn open_blocking(path: &str) -> Result<Self, String> {
        Self::open_inner(path, true, None)
    }

    /// v6: open with a WATCHDOG TIMEOUT. A foreign / older on-disk format (e.g. a
    /// v4.1-era store opened by a v5/v6 binary) can make the underlying
    /// `flux_db::Database::open` block indefinitely — the node then sits forever at
    /// "opening block store" with no progress and no error, so the caller's
    /// fresh-store fallback (which only fires on `Err`) never engages. This runs the
    /// open on a worker thread and gives up after `secs`, returning `Err` so the
    /// caller falls back to a clean store instead of hanging the whole node. The
    /// worker thread is left blocked (harmless: the process restarts onto the fresh
    /// store). `flux_db::Database` is already `Send` (it is moved into the background
    /// migrate thread), so `BlockStore` crosses the channel cleanly.
    pub fn open_with_timeout(path: &str, secs: u64) -> Result<Self, String> {
        Self::open_with_timeout_and_progress(path, secs, None)
    }

    /// Same as [`Self::open_with_timeout`], but reports live WAL-replay progress into
    /// `progress` (if given) while the open runs on its watchdog thread — the caller
    /// can poll `progress.snapshot()` from a render loop without waiting on `rx`.
    pub fn open_with_timeout_and_progress(
        path: &str,
        secs: u64,
        progress: Option<std::sync::Arc<OpenProgress>>,
    ) -> Result<Self, String> {
        let p = path.to_string();
        let (tx, rx) = std::sync::mpsc::channel::<Result<Self, String>>();
        std::thread::Builder::new()
            .name("sigil-blockstore-open".into())
            .spawn(move || {
                let r = Self::open_inner(&p, false, progress.as_ref());
                if let Some(pr) = &progress {
                    pr.mark_finished();
                }
                let _ = tx.send(r);
            })
            .map_err(|e| format!("spawn open watchdog: {e}"))?;
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(r) => r,
            Err(_) => Err(format!(
                "open timed out after {secs}s (incompatible or locked store — falling back to a fresh store)"
            )),
        }
    }

    fn open_inner(path: &str, inline_migration: bool, progress: Option<&std::sync::Arc<OpenProgress>>) -> Result<Self, String> {
        let db_path = PathBuf::from(path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }
        let db = match progress {
            Some(pr) => {
                let pr = pr.clone();
                flux_db::Database::open_with_progress(&db_path, move |consumed, total| pr.set(consumed, total))?
            }
            None => flux_db::Database::open(&db_path)?,
        };

        // v0.35 fast open: best_(height,hash) persists under meta 'B'; when present we
        // skip the scan entirely. Stores from before 'B' migrate ONCE (self-migrating).
        let mut best_height = 0u64;
        let mut best_hash_hex = String::new();
        let mut need_best_migration = true;
        if let Some(v) = db.get(&[KEY_META, b'B']).ok().flatten() {
            if v.len() >= 8 {
                if let Ok(hb) = <[u8; 8]>::try_from(&v[..8]) {
                    best_height = u64::from_be_bytes(hb);
                    best_hash_hex = String::from_utf8_lossy(&v[8..]).into_owned();
                    need_best_migration = false;
                }
            }
        }
        if need_best_migration {
            // INSTANT-BOOT FIX. The legacy migration was a synchronous full-db scan
            // (bincode-deserialize every block) that ran HERE, before the first TUI
            // paint — minutes on a multi-GB store, an OOM-class RAM spike that lagged
            // the whole machine, and because 'B' was only written AFTER the loop,
            // closing mid-scan saved nothing so every launch re-scanned (= "5 minutes
            // and it never gets faster"). Now it runs on a BACKGROUND thread over a
            // cloned handle (flux_db::Database is Arc<RwLock> inside → the clone shares
            // storage and writes are visible), checkpointing 'B' every CHECKPOINT
            // blocks so an interrupt still leaves the next open O(1). open() returns at
            // once; the light monitor paints immediately while the cache index rebuilds
            // quietly. Inline only for headless tooling / tests (need best_height now).
            if inline_migration || cfg!(test) {
                let (h, hash) = Self::migrate_index(&db);
                best_height = h;
                best_hash_hex = hash;
            } else {
                let bg = db.clone();
                let _ = std::thread::Builder::new()
                    .name("sigil-blockstore-migrate".into())
                    .spawn(move || {
                        let _ = Self::migrate_index(&bg);
                    });
            }
        }

        let synced_to = db
            .get(&[KEY_META, b'S']).ok().flatten()
            .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok().map(u64::from_be_bytes))
            .unwrap_or(0);
        // v3 (LANE-C): the persisted fold anchor (meta 'A' = be(height) ++ 32B hash) — loaded
        // BEFORE the verified_to clamp so the clamp-relax it grants survives reopen.
        let fold_anchor: Option<(u64, BlockHash)> = db
            .get(&[KEY_META, b'A']).ok().flatten()
            .filter(|v| v.len() == 40)
            .and_then(|v| {
                let h = u64::from_be_bytes(<[u8; 8]>::try_from(&v[..8]).ok()?);
                let hash = <[u8; 32]>::try_from(&v[8..40]).ok()?;
                Some((h, hash))
            });
        let fold_h = fold_anchor.map(|(h, _)| h).unwrap_or(0);
        let verified_to = db
            .get(&[KEY_META, b'V']).ok().flatten()
            .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok().map(u64::from_be_bytes))
            .unwrap_or(0)
            // never claim verified past what's downloaded OR fold-anchored (the fold prefix is
            // trusted-not-downloaded, so verified_to may legitimately exceed synced_to up to the anchor).
            .min(synced_to.max(fold_h));

        // LANE-S: the genesis-anchor hash the persisted watermarks belong to.
        let genesis_hash = db
            .get(&[KEY_META, b'G']).ok().flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let sst_ingest = std::env::var("SIGIL_DB_SST_INGEST").ok()
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false")).unwrap_or(false);
        let mut s = BlockStore {
            db, best_height, best_hash_hex, synced_to, verified_to, base: 0, genesis_hash, fold_anchor,
            link_cache: LinkCache::from_env(), sst_ingest,
        };
        s.advance_synced(); // catch up the contiguous pointer to whatever's on disk
        Ok(s)
    }

    /// One-time index migration for a pre-'B' store. Rebuilds the height index and
    /// persists best-(height,hash) under meta 'B', CHECKPOINTING every `CHECKPOINT`
    /// blocks so an interrupted run still leaves the next open O(1) (the trap that
    /// made boots permanently slow when the user closed mid-scan). `put` is `&self`
    /// and `db.iter()` returns an owned snapshot, so this is safe to run on a cloned
    /// handle off the main thread. Returns (best_height, best_hash_hex).
    fn migrate_index(db: &flux_db::Database) -> (u64, String) {
        const CHECKPOINT: u64 = 50_000;
        let (mut best_height, mut best_hash_hex) = (0u64, String::new());
        let mut have_index = false;
        let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // (height_key, hash_key), flushed per checkpoint
        let mut seen: u64 = 0;
        let checkpoint = |db: &flux_db::Database, pending: &mut Vec<(Vec<u8>, Vec<u8>)>, h: u64, hash: &str| {
            for (hk, hashk) in pending.drain(..) {
                let _ = db.put(&hk, &hashk);
            }
            let mut bv = h.to_be_bytes().to_vec();
            bv.extend_from_slice(hash.as_bytes());
            let _ = db.put(&[KEY_META, b'B'], &bv);
        };
        for (key, value) in db.iter() {
            match key.first() {
                Some(&KEY_HINDEX) => { have_index = true; continue; }
                Some(&KEY_META) => continue,
                _ => {}
            }
            if let Ok(block) = bincode::deserialize::<StoredBlock>(&value) {
                let h = block.header.height;
                if h >= best_height {
                    best_height = h;
                    best_hash_hex = block.hash_hex.clone();
                }
                // Only rebuild the height index for a store that never had one.
                if !have_index {
                    pending.push((height_key(h), key));
                }
                seen += 1;
                if seen % CHECKPOINT == 0 {
                    checkpoint(db, &mut pending, best_height, &best_hash_hex);
                }
            }
        }
        checkpoint(db, &mut pending, best_height, &best_hash_hex);
        (best_height, best_hash_hex)
    }

    /// v0.35: persist best_(height,hash) under meta 'B' — the key that makes open() O(1)
    /// instead of a full-db scan. Called by every put path that can raise best_height.
    fn persist_best(&mut self) {
        let mut v = self.best_height.to_be_bytes().to_vec();
        v.extend_from_slice(self.best_hash_hex.as_bytes());
        let _ = self.db.put(&[KEY_META, b'B'], &v);
    }

    /// True if a block at `height` is stored (via the height index).
    pub fn has_height(&self, height: u64) -> bool {
        self.db.get(&height_key(height)).ok().flatten().is_some()
    }

    fn hash_at_index_height(&self, height: u64) -> Option<String> {
        self.db
            .get(&height_key(height))
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v).ok())
    }

    fn height_index_conflict(&self, height: u64, incoming_hash_hex: &str) -> Option<String> {
        self.hash_at_index_height(height)
            .filter(|existing| existing != incoming_hash_hex)
    }

    /// THROUGHPUT_MASTER LANE 2: cache-accelerated height-index conflict check. Outcome-IDENTICAL
    /// to `height_index_conflict` (only ever DROPS a redundant `db.get`):
    ///   • cache hit  → the cached hash IS the stored hash → compare in memory (no get).
    ///   • cache miss but `height > best_height` → nothing was ever stored at/above the max stored
    ///     height (within this batch, dup-at-height is caught by `batch_seen`), so the disk index is
    ///     a guaranteed MISS → skip the get.
    ///   • otherwise   → fall through to the disk index (`height ≤ best_height`, possibly a gap/fork).
    /// When the cache is disabled this is exactly `height_index_conflict` (disk every time).
    fn height_index_conflict_cached(&self, height: u64, incoming_hash_hex: &str) -> Option<String> {
        if self.link_cache.enabled {
            if let Some(existing) = self.link_cache.get(height) {
                return (existing != incoming_hash_hex).then(|| existing.clone());
            }
            if height > self.best_height {
                return None; // never stored at/above best_height → no disk conflict possible
            }
        }
        self.height_index_conflict(height, incoming_hash_hex)
    }

    /// THROUGHPUT_MASTER LANE 2 (test/integration knob): toggle the in-memory parent-hash cache.
    /// Production reads `SIGIL_COMMIT_PARENT_CACHE` at open; this lets a test exercise both paths
    /// without env juggling. Clears the map when turned off so stale entries can't leak.
    pub fn set_link_cache(&mut self, enabled: bool) {
        self.link_cache.enabled = enabled;
        if !enabled { self.link_cache.clear(); }
    }

    /// THROUGHPUT_MASTER LANE 2 (test/integration knob): toggle the flux-db SST-ingest path for
    /// `commit_bulk_trusted_durable` (production reads `SIGIL_DB_SST_INGEST` at open).
    pub fn set_sst_ingest(&mut self, enabled: bool) { self.sst_ingest = enabled; }

    fn linkage_conflict(&self, height: u64, hash_hex: &str, parent_hash_hex: &str) -> Option<String> {
        if height > self.base {
            match self.get_stored_at_height(height - 1) {
                Some(parent) => {
                    if !parent.hash_hex.eq_ignore_ascii_case(parent_hash_hex) {
                        return Some(format!(
                            "parent h={} hash={} but incoming parent_hash={}",
                            height - 1,
                            short_hash_hex(&parent.hash_hex),
                            short_hash_hex(parent_hash_hex)
                        ));
                    }
                }
                // v0.95 STRICT DOWNWARD-LINKAGE: refuse a block whose parent isn't stored yet.
                // This is the only ingest window with no parent to link against, and it was the
                // frontier-wedge ROOT: an out-of-order "squatter" at H (stored before its parent)
                // both blocked the canonical H via the height index AND rejected the canonical H-1
                // via the child-linkage check below — non-deterministically wedging full-archive.
                // Refusing it makes squatters impossible; a contiguous frontier chunk always has
                // its parent present, so legitimate sync is unaffected (the chunk is re-fetched in
                // order if it ever arrives early). Genesis-anchor `base` blocks are exempt above.
                None => {
                    return Some(format!(
                        "parent h={} not yet stored (strict downward-linkage: refusing out-of-order squatter)",
                        height - 1
                    ));
                }
            }
        }
        if height <= self.synced_to {
            if let Some(child) = self.get_stored_at_height(height.saturating_add(1)) {
                let child_parent = hex::encode(child.header.parent_hash);
                if !hash_hex.eq_ignore_ascii_case(&child_parent) {
                    return Some(format!(
                        "child h={} expects parent={} but incoming hash={}",
                        height.saturating_add(1),
                        short_hash_hex(&child_parent),
                        short_hash_hex(hash_hex)
                    ));
                }
            }
        }
        None
    }

    /// Advance the contiguous pointer over any consecutive heights present, persisting
    /// it. Called after every store + once on open. Starts at `base` (the genesis anchor):
    /// heights below `base` are never required (not backfill-servable), so the frontier is
    /// clamped up to `base` before walking.
    fn advance_synced(&mut self) {
        if self.synced_to < self.base { self.synced_to = self.base; }
        let mut moved = false;
        while self.has_height(self.synced_to) {
            self.synced_to += 1;
            moved = true;
        }
        if moved {
            let _ = self.db.put(&[KEY_META, b'S'], &self.synced_to.to_be_bytes());
        }
    }

    /// v0.10.0: The genesis-anchor height — the lowest servable height. Blocks below this are
    /// never required for "fully synced". See the `base` field doc.
    pub fn base(&self) -> u64 { self.base }

    /// v0.10.0: Set the genesis anchor (e.g. 1 for SIGIL, whose height-0 genesis isn't
    /// backfill-servable). Bumps `synced_to`/`verified_to` up to `base` (heights below it are
    /// never needed) and persists the new frontier. Call once after `open`, before sync.
    pub fn set_base(&mut self, base: u64) {
        self.base = base;
        if self.synced_to < base {
            self.synced_to = base;
            let _ = self.db.put(&[KEY_META, b'S'], &self.synced_to.to_be_bytes());
        }
        if self.verified_to < base { self.set_verified_to(base); }
        self.advance_synced();
    }

    /// 0.77 GENESIS ARCHIVE: re-anchor a snapped store back DOWN to `base` so the
    /// contiguous frontier re-walks genesis→tip and the store HOLDS every block.
    /// `set_base` can only RAISE the watermarks (its job is the one-time genesis
    /// anchor); flipping a recent-window store into a full archive needs
    /// `synced_to`/`verified_to` LOWERED to the anchor or they keep claiming a
    /// contiguity the disk doesn't have. Blocks already stored (the recent window)
    /// stay put — `advance_synced` sweeps any consecutive run at the anchor, and
    /// the frontier absorbs the rest as out-of-order arrivals when it reaches them.
    pub fn rebase(&mut self, base: u64) {
        self.base = base;
        self.synced_to = base;
        let _ = self.db.put(&[KEY_META, b'S'], &self.synced_to.to_be_bytes());
        self.verified_to = base;
        let _ = self.db.put(&[KEY_META, b'V'], &self.verified_to.to_be_bytes());
        self.link_cache.clear(); // LANE 2: re-anchoring invalidates the committed-height cache
        self.advance_synced();
    }

    /// v7.0.18 FRONTIER SELF-HEAL: distrust our own stored frontier seam. When honest
    /// frontier chunks repeatedly arrive but refuse to splice (parent-hash mismatch against
    /// what WE hold), the poisoned block is LOCAL — typically written by a bulk/skeleton
    /// lane (`put_blocks_bulk_trusted` does no linkage checking) from a corrupt WAN page.
    /// Roll the contiguous frontier back `back` heights and DROP the height index for
    /// [new_frontier, old_frontier+70k] (one range tombstone; the 70k overhang covers any
    /// bulk-page squatters above the seam, while leaving live-tip blocks alone). Bodies stay
    /// (hash-keyed, harmless orphans); `put_block`/`put_blocks_batch` re-link on refetch.
    /// Returns the new frontier.
    pub fn rollback_frontier(&mut self, back: u64) -> u64 {
        let old = self.synced_to;
        let new = old.saturating_sub(back).max(self.base);
        let hi_end = old.saturating_add(70_000);
        // Per-key point deletes, NOT delete_range: flux-db range tombstones shadow
        // FUTURE writes in the range too (is_covered ignores seq), so a refetched
        // index entry would read back as absent and the heal would wedge at the
        // rollback point (proven by rollback_frontier_heals_poisoned_seam under
        // delete_range). A point tombstone is simply overwritten by the re-put.
        for h in new..hi_end {
            let _ = self.db.delete(&height_key(h));
        }
        self.synced_to = new;
        let _ = self.db.put(&[KEY_META, b'S'], &new.to_be_bytes());
        if self.verified_to > new { self.set_verified_to(new); }
        self.link_cache.clear(); // the cache may hold the poisoned seam's hashes
        let _ = self.db.flush();
        new
    }

    /// Contiguous synced count: blocks base..synced_to are all present (next needed = this).
    pub fn synced_to(&self) -> u64 { self.synced_to }

    /// v0.9.0: Contiguous cryptographically-verified count: blocks 0..verified_to have
    /// each passed precheck + parent linkage. The "full verifying sync" watermark.
    pub fn verified_to(&self) -> u64 { self.verified_to }

    /// v0.9.0: Persist the verified watermark. Clamped to `synced_to` (can't verify what
    /// isn't downloaded) and monotonic guard is the caller's job — the verifier only ever
    /// advances it. Cheap no-op if unchanged.
    pub fn set_verified_to(&mut self, h: u64) {
        // v3 (LANE-C): clamp to downloaded OR fold-anchored — below an active fold anchor the
        // prefix is trusted-not-downloaded, so verified_to may exceed synced_to up to the anchor.
        let h = h.min(self.synced_to.max(self.fold_anchor_height()));
        if h != self.verified_to {
            self.verified_to = h;
            let _ = self.db.put(&[KEY_META, b'V'], &self.verified_to.to_be_bytes());
        }
    }

    /// v3 (LANE-C): the active fold anchor height (0 if none) — the ceiling verified_to may reach
    /// without downloading every block below it.
    pub fn fold_anchor_height(&self) -> u64 { self.fold_anchor.map(|(h, _)| h).unwrap_or(0) }
    /// v3 (LANE-C): the active fold anchor `(height, hash)`, if any.
    pub fn fold_anchor(&self) -> Option<(u64, BlockHash)> { self.fold_anchor }

    /// LANE-S: a chain reset (fresh genesis) invalidates EVERY watermark — they describe the
    /// OLD chain's blocks, which no longer exist on the network. Without this the persisted
    /// synced_to/verified_to/best_height survive the reset and the SYNC hero keeps showing a
    /// phantom checkpoint (e.g. 5M) while the fresh tip is 0.39M, until a manual local wipe.
    /// Zero every watermark + persist so the store re-downloads from the fresh genesis. Stale
    /// block bodies are harmless — they get overwritten by height as the new chain syncs in.
    pub fn reset_watermarks(&mut self) {
        self.synced_to = 0;
        self.verified_to = 0;
        self.best_height = 0;
        self.best_hash_hex = String::new();
        self.base = 0;
        self.genesis_hash = String::new(); // LANE-S: forget the dead chain's genesis anchor
        self.fold_anchor = None;           // v3 LANE-C: the dead chain's fold anchor is invalid too
        self.link_cache.clear();           // LANE 2: dead chain's height→hash entries are invalid
        let _ = self.db.put(&[KEY_META, b'S'], &0u64.to_be_bytes());
        let _ = self.db.put(&[KEY_META, b'V'], &0u64.to_be_bytes());
        let _ = self.db.put(&[KEY_META, b'G'], b""); // clear the persisted genesis key
        let _ = self.db.put(&[KEY_META, b'A'], b""); // v3: clear the persisted fold anchor
        self.persist_best(); // write best_height=0 under meta 'B'
    }

    /// LANE-S: the genesis-anchor hash the persisted watermarks belong to ("" until first seen).
    pub fn genesis_hash(&self) -> &str { &self.genesis_hash }

    /// LANE-S: key the watermarks to the LIVE genesis anchor (the hash of the block at `base`) and
    /// AUTO-INVALIDATE on a mismatch. Returns `true` ONLY when it detects a CHANGED genesis — i.e.
    /// a testnet restart minted a fresh chain — in which case it has ALREADY wiped the now-stale
    /// watermarks (`reset_watermarks`) and adopted the new anchor; the caller then drops the
    /// last-tip cache + resets the in-memory peer_best/verified so the hero self-heals to the
    /// fresh tip with NO manual local wipe. No-op (false) on unknown/first/unchanged genesis.
    pub fn note_genesis(&mut self, live_hash: &str) -> bool {
        let live = live_hash.trim();
        if live.is_empty() {
            return false;
        }
        if self.genesis_hash.is_empty() {
            self.genesis_hash = live.to_string();
            let _ = self.db.put(&[KEY_META, b'G'], self.genesis_hash.as_bytes());
            return false;
        }
        if self.genesis_hash == live {
            return false;
        }
        // Fresh genesis under our feet → the persisted watermarks describe a DEAD chain. Wipe them,
        // then adopt the new anchor (reset_watermarks cleared genesis_hash, so set it AFTER).
        self.reset_watermarks();
        self.genesis_hash = live.to_string();
        let _ = self.db.put(&[KEY_META, b'G'], self.genesis_hash.as_bytes());
        true
    }

    /// v0.9.0: Load the full stored header at a given height (via the height index →
    /// hash → block). Returns None if that height isn't stored. This is the read path the
    /// chain verifier walks to recompute `hash()` and check `parent_hash` linkage.
    pub fn get_header_at_height(&self, height: u64) -> Option<SigilBlockHeaderV0> {
        let hashk = self.db.get(&height_key(height)).ok().flatten()?;
        let hash_hex = String::from_utf8(hashk).ok()?;
        self.get_block(&hash_hex).map(|b| b.header)
    }

    /// v0.33 (1M-blk/s lane): like [`Self::get_header_at_height`] but returns the FULL
    /// [`StoredBlock`] — crucially including `hash_hex`, the block hash computed ONCE at
    /// ingest. `SigilBlockHeaderV0::hash()` JSON-serializes the entire ~1 KB header (≈3-5 KB
    /// of JSON text) per call (~15-25 µs); the verifier used to recompute it for EVERY
    /// parent-linkage check, doubling per-block verify cost. Reusing the stored hash makes
    /// linkage a 32-byte compare. (Sound: `hash_hex` was produced by OUR ingest calling
    /// `header.hash()` on these exact stored bytes — it IS that hash, cached.)
    pub fn get_stored_at_height(&self, height: u64) -> Option<StoredBlock> {
        let hashk = self.db.get(&height_key(height)).ok().flatten()?;
        let hash_hex = String::from_utf8(hashk).ok()?;
        self.get_block(&hash_hex)
    }

    pub fn put_block(&mut self, header: SigilBlockHeaderV0) -> Result<bool, String> {
        let hash = header.hash();
        let hash_hex = hex::encode(hash);
        let height = header.height;

        if let Some(existing) = self.height_index_conflict(height, &hash_hex) {
            crate::tlog!(
                "[store] rejected height-index fork overwrite at h={height}: existing={} incoming={}",
                short_hash_hex(&existing),
                short_hash_hex(&hash_hex)
            );
            return Ok(false);
        }
        let parent_hash_hex = hex::encode(header.parent_hash);
        if let Some(reason) = self.linkage_conflict(height, &hash_hex, &parent_hash_hex) {
            crate::tlog!("[store] rejected unlinked header at h={height}: {reason}");
            return Ok(false);
        }

        if self.db.get(hash_hex.as_bytes())?.is_some() {
            // v7.0.18: body already stored but the height index may be gone (a frontier
            // rollback deletes ONLY the index). Re-link it so a refetch after self-heal
            // can advance — the conflict/linkage checks above already passed for this
            // (height, hash). Without this, re-ingest silently no-ops and the heal wedges.
            if self.db.get(&height_key(height)).ok().flatten().is_none() {
                self.db.put(&height_key(height), hash_hex.as_bytes())?;
                self.advance_synced();
                return Ok(true);
            }
            return Ok(false);
        }

        let block = StoredBlock {
            header,
            hash_hex: hash_hex.clone(),
            synced_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };

        let value = bincode::serialize(&block).map_err(|e| format!("serialize: {}", e))?;
        self.db.put(hash_hex.as_bytes(), &value)?;
        self.db.put(&height_key(height), hash_hex.as_bytes())?; // height index

        if height >= self.best_height {
            self.best_height = height;
            self.best_hash_hex = hash_hex;
            self.persist_best(); // v0.35: keep open() O(1)
        }
        self.advance_synced(); // extend the contiguous pointer if this filled the next gap

        Ok(true)
    }

    /// Fast backfill store: no dupe-check read (overlapping chunks just overwrite —
    /// idempotent), writes block + height index only. Does NOT advance the contiguous
    /// pointer — call [`Self::advance`] ONCE after a batch (saves per-block work that
    /// dominated ingest time). Skips the per-block `db.get` + `get_block` round-trips.
    pub fn put_block_fast(&mut self, header: SigilBlockHeaderV0) -> Result<(), String> {
        let hash_hex = hex::encode(header.hash());
        let height = header.height;
        if let Some(existing) = self.height_index_conflict(height, &hash_hex) {
            crate::tlog!(
                "[store] rejected fast height-index fork overwrite at h={height}: existing={} incoming={}",
                short_hash_hex(&existing),
                short_hash_hex(&hash_hex)
            );
            return Ok(());
        }
        let parent_hash_hex = hex::encode(header.parent_hash);
        if let Some(reason) = self.linkage_conflict(height, &hash_hex, &parent_hash_hex) {
            crate::tlog!("[store] rejected fast unlinked header at h={height}: {reason}");
            return Ok(());
        }
        let block = StoredBlock { header, hash_hex: hash_hex.clone(), synced_at: 0 };
        let value = bincode::serialize(&block).map_err(|e| format!("serialize: {}", e))?;
        self.db.put(hash_hex.as_bytes(), &value)?;
        self.db.put(&height_key(height), hash_hex.as_bytes())?;
        if height >= self.best_height {
            self.best_height = height;
            self.best_hash_hex = hash_hex;
            self.persist_best(); // v0.35: keep open() O(1)
        }
        Ok(())
    }

    /// v0.10.0: Batched fast store — write a whole chunk's blocks in ONE `batch_put`
    /// (single WAL-lock hold) instead of 2 locked `put`s per block. The per-block path
    /// (4096 lock acquires per 2048-block chunk) dominated ingest time under load; batching
    /// collapses it to one. Returns how many blocks were written. Does NOT advance the
    /// contiguous pointer — call [`Self::advance`] once after.
    pub fn put_blocks_batch(&mut self, headers: &[SigilBlockHeaderV0]) -> usize {
        if headers.is_empty() { return 0; }
        // v0.33 (1M-blk/s lane): hash + serialize IN PARALLEL. `header.hash()` JSON-encodes
        // the whole ~1 KB header (≈3-5 KB text → ~15-25 µs each) — at CHUNK=4096 that's
        // ~60-100 ms of pure CPU per chunk, serialized on the sync-loop thread. Every
        // header is independent, so fan the (hash, bincode) work across cores with scoped
        // threads (no new dep); only the single batch_put stays serial. On the 48-core
        // box this turns the ingest CPU wall from ~25 µs/blk into ~1-2 µs/blk.
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get()).unwrap_or(4)
            .min(16)                       // diminishing returns past the memory bandwidth
            .min(headers.len().max(1));
        let chunk_sz = headers.len().div_ceil(nthreads);
        // per-header (height, hash_hex, bincode(StoredBlock)) — computed in parallel.
        let mut prepared: Vec<(u64, String, String, Vec<u8>)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(nthreads);
            for chunk in headers.chunks(chunk_sz) {
                handles.push(s.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for header in chunk {
                        let hash_hex = hex::encode(header.hash());
                        let parent_hash_hex = hex::encode(header.parent_hash);
                        let block = StoredBlock {
                            header: header.clone(), hash_hex: hash_hex.clone(), synced_at: 0,
                        };
                        if let Ok(value) = bincode::serialize(&block) {
                            out.push((block.header.height, hash_hex, parent_hash_hex, value));
                        }
                    }
                    out
                }));
            }
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        prepared.sort_by_key(|(height, _, _, _)| *height);
        let batch_heights: std::collections::HashSet<u64> =
            prepared.iter().map(|(height, _, _, _)| *height).collect();
        // Serial tail: assemble the (key, value) pairs + ONE batch_put (single WAL hold).
        // Never let an overlapping/forked response rewrite the height index for a
        // height we already mapped. The verifier assumes a height points to one stable
        // spine; replacing h=N after h=N+1 has landed strands the parent-linkage walk.
        let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(prepared.len() * 2);
        let mut max_h = self.best_height;
        let mut max_hash = self.best_hash_hex.clone();
        let mut accepted = 0usize;
        let mut conflicts = 0usize;
        let mut first_conflict: Option<(u64, String, String)> = None;
        let mut batch_seen: std::collections::HashMap<u64, String> =
            std::collections::HashMap::with_capacity(prepared.len());
        let mut accepted_hash_by_height: std::collections::HashMap<u64, String> =
            std::collections::HashMap::with_capacity(headers.len());
        // THROUGHPUT_MASTER LANE 2: heights accepted by THIS batch, recorded into the parent-hash
        // cache only AFTER the batch_put commits (so the cache never claims an un-committed height).
        let mut cache_updates: Vec<(u64, String)> = Vec::new();
        for (height, hash_hex, parent_hash_hex, value) in prepared {
            let conflict = batch_seen
                .get(&height)
                .filter(|existing| *existing != &hash_hex)
                .cloned()
                .or_else(|| self.height_index_conflict_cached(height, &hash_hex));
            if let Some(existing) = conflict {
                conflicts += 1;
                first_conflict.get_or_insert_with(|| (height, existing, hash_hex));
                continue;
            }
            let mut link_conflict = None;
            if height > self.base {
                if let Some(parent_hash) = accepted_hash_by_height.get(&(height - 1)) {
                    if !parent_hash.eq_ignore_ascii_case(&parent_hash_hex) {
                        link_conflict = Some(format!(
                            "batch parent h={} hash={} but incoming parent_hash={}",
                            height - 1,
                            short_hash_hex(parent_hash),
                            short_hash_hex(&parent_hash_hex)
                        ));
                    }
                } else if batch_heights.contains(&(height - 1)) {
                    link_conflict = Some(format!("batch parent h={} was rejected or missing", height - 1));
                } else if height > self.synced_to {
                    // THROUGHPUT_MASTER LANE 2 — pure frontier: `linkage_conflict`'s child-check (h+1)
                    // is a guaranteed no-op above `synced_to`, so a parent-cache hit is byte-identical
                    // to the disk get (the cached hash IS the stored parent's hash). Drop the get on a
                    // hit; on a miss fall through to disk (preserves the strict-downward squatter refusal).
                    match self.link_cache.get(height - 1) {
                        Some(cached_parent) => {
                            if !cached_parent.eq_ignore_ascii_case(&parent_hash_hex) {
                                link_conflict = Some(format!(
                                    "cached parent h={} hash={} but incoming parent_hash={}",
                                    height - 1,
                                    short_hash_hex(cached_parent),
                                    short_hash_hex(&parent_hash_hex)
                                ));
                            }
                        }
                        None => {
                            if let Some(reason) = self.linkage_conflict(height, &hash_hex, &parent_hash_hex) {
                                link_conflict = Some(reason);
                            }
                        }
                    }
                } else if let Some(reason) = self.linkage_conflict(height, &hash_hex, &parent_hash_hex) {
                    link_conflict = Some(reason);
                }
            } else if let Some(reason) = self.linkage_conflict(height, &hash_hex, &parent_hash_hex) {
                link_conflict = Some(reason);
            }
            if let Some(reason) = link_conflict {
                conflicts += 1;
                first_conflict.get_or_insert_with(|| (height, reason, hash_hex));
                continue;
            }
            batch_seen.entry(height).or_insert_with(|| hash_hex.clone());
            accepted_hash_by_height.entry(height).or_insert_with(|| hash_hex.clone());
            accepted += 1;
            if self.link_cache.enabled { cache_updates.push((height, hash_hex.clone())); }
            owned.push((hash_hex.clone().into_bytes(), value));              // block by hash
            owned.push((height_key(height), hash_hex.clone().into_bytes())); // height index
            if height >= max_h { max_h = height; max_hash = hash_hex; }
        }
        if let Some((height, existing, incoming)) = first_conflict {
            crate::tlog!(
                "[store] rejected {conflicts} batch header conflict(s); first h={height}: detail={} incoming={}",
                existing,
                short_hash_hex(&incoming)
            );
        }
        if owned.is_empty() {
            return accepted;
        }
        let refs: Vec<(&[u8], &[u8])> = owned.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
        match self.db.batch_put(&refs) {
            Ok(()) => {
                self.best_height = max_h;
                self.best_hash_hex = max_hash;
                self.persist_best(); // v0.35: ONE meta put per batch keeps open() O(1)
                // LANE 2: now-durable heights enter the parent-hash cache (no-op if disabled).
                for (h, hh) in cache_updates { self.link_cache.record(h, &hh); }
                accepted
            }
            Err(_) => 0,
        }
    }

    /// v3 (LANE-C, the 100k bulk path): commit a VERIFIED, CONTIGUOUS, fold-proven prefix with
    /// ZERO per-block gets. `put_blocks_batch` pays a guaranteed-miss `db.get` per block
    /// (`height_index_conflict` + dupe-check) — the read_dir-per-miss that capped commit at ~3.3k
    /// blk/s. Even with flux-db's SST-list cache that's still a lock + bloom probe per block; for a
    /// prefix LANE-B's fold fast-path already authenticated against the signed anchor, those checks
    /// are pure overhead. This goes straight to ONE `batch_put`: parallel hash+serialize, assemble
    /// the (block-by-hash, height-index) pairs, single WAL-locked write, bump best.
    ///
    /// ⚠️ CALLER MUST GUARANTEE the headers are verified + contiguous + within the fold anchor —
    /// it does NO fork / dup / linkage checking. Use ONLY for the trusted snapshot/skeleton prefix;
    /// live gossip + the frontier still go through `put_blocks_batch` (which checks). Does NOT
    /// advance the contiguous pointer — call [`Self::advance`] after. Returns blocks written.
    pub fn put_blocks_bulk_trusted(&mut self, headers: &[SigilBlockHeaderV0]) -> usize {
        if headers.is_empty() { return 0; }
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get()).unwrap_or(4)
            .min(16)
            .min(headers.len().max(1));
        let chunk_sz = headers.len().div_ceil(nthreads);
        // Parallel hash + bincode across cores (same as put_blocks_batch); only the batch_put serial.
        let mut prepared: Vec<(u64, String, Vec<u8>)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(nthreads);
            for chunk in headers.chunks(chunk_sz) {
                handles.push(s.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for header in chunk {
                        let hash_hex = hex::encode(header.hash());
                        let block = StoredBlock { header: header.clone(), hash_hex: hash_hex.clone(), synced_at: 0 };
                        if let Ok(value) = bincode::serialize(&block) {
                            out.push((header.height, hash_hex, value));
                        }
                    }
                    out
                }));
            }
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        prepared.sort_by_key(|(height, _, _)| *height);
        let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(prepared.len() * 2);
        let mut max_h = self.best_height;
        let mut max_hash = self.best_hash_hex.clone();
        let mut cache_updates: Vec<(u64, String)> = Vec::new();
        let n = prepared.len();
        for (height, hash_hex, value) in prepared {
            if self.link_cache.enabled { cache_updates.push((height, hash_hex.clone())); }
            owned.push((hash_hex.clone().into_bytes(), value));              // block by hash
            owned.push((height_key(height), hash_hex.clone().into_bytes())); // height index
            if height >= max_h { max_h = height; max_hash = hash_hex; }
        }
        let refs: Vec<(&[u8], &[u8])> = owned.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
        match self.db.batch_put(&refs) {
            Ok(()) => {
                self.best_height = max_h;
                self.best_hash_hex = max_hash;
                self.persist_best();
                // LANE 2: feed the trusted prefix's tail into the cache so the first checked frontier
                // batch links to it in memory (the trusted path itself does no linkage checks).
                for (h, hh) in cache_updates { self.link_cache.record(h, &hh); }
                n
            }
            Err(_) => 0,
        }
    }

    /// v3 (LANE-C): durable trusted-bulk commit — `put_blocks_bulk_trusted` + the 2-phase atomic-tip
    /// fsync (see `commit_batch_durable`). The fast path for the snapshot/skeleton prefix.
    pub fn commit_bulk_trusted_durable(&mut self, headers: &[SigilBlockHeaderV0], fsync: bool) -> Result<usize, String> {
        if headers.is_empty() { return Ok(0); }
        let n = if self.sst_ingest {
            self.put_blocks_bulk_trusted_ingest(headers) // LANE 1 SST-ingest seam (falls back today)
        } else {
            self.put_blocks_bulk_trusted(headers)        // memtable+WAL, no per-block gets
        };
        if fsync { self.db.sync_wal()?; }              // blocks durable
        self.advance_synced();                         // tip 'S' → WAL
        if fsync { self.db.sync_wal()?; }              // tip durable after blocks
        Ok(n)
    }

    /// THROUGHPUT_MASTER LANE 2 ⇄ LANE 1 SEAM: commit the verified contiguous prefix via flux-db's
    /// off-thread sorted-SST builder + atomic ingest (`ingest_sorted_bodies`), skipping the
    /// WAL+memtable+compaction path that caps `batch_put` at ~4k blk/s. Gated by `SIGIL_DB_SST_INGEST`
    /// (`self.sst_ingest`), and only reachable from `commit_bulk_trusted_durable` — i.e. ONLY for the
    /// fold/anchor-verified prefix the trusted route already guards.
    ///
    /// L1's `flux_db::Database::ingest_sorted_bodies` is now LANDED (flux 921352ea); this WIRES it:
    /// build the globally-key-sorted (block-by-hash, height-index) pairs and call
    /// `self.db.ingest_sorted_bodies(&owned)` (off-WAL SST build + atomic install); batch_put fallback
    /// only on ingest error. The 2-phase tip-after-bodies fsync stays in `commit_bulk_trusted_durable`,
    /// so atomic ingest + tip-after-bodies preserve kill-9 crash-safety.
    fn put_blocks_bulk_trusted_ingest(&mut self, headers: &[SigilBlockHeaderV0]) -> usize {
        if headers.is_empty() { return 0; }
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get()).unwrap_or(4)
            .min(16)
            .min(headers.len().max(1));
        let chunk_sz = headers.len().div_ceil(nthreads);
        // Parallel hash + bincode across cores (identical to put_blocks_bulk_trusted); only the
        // serial SST build/install runs on the commit thread.
        let prepared: Vec<(u64, String, Vec<u8>)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(nthreads);
            for chunk in headers.chunks(chunk_sz) {
                handles.push(s.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for header in chunk {
                        let hash_hex = hex::encode(header.hash());
                        let block = StoredBlock { header: header.clone(), hash_hex: hash_hex.clone(), synced_at: 0 };
                        if let Ok(value) = bincode::serialize(&block) {
                            out.push((header.height, hash_hex, value));
                        }
                    }
                    out
                }));
            }
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        let n = prepared.len();
        let mut max_h = self.best_height;
        let mut max_hash = self.best_hash_hex.clone();
        let mut cache_updates: Vec<(u64, String)> = Vec::new();
        // Build BOTH keyspaces, then GLOBALLY key-sort: flux-db's SST builder requires strictly
        // key-sorted input (put_blocks_bulk_trusted sorts by height, which only batch_put tolerates).
        let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(n * 2);
        for (height, hash_hex, value) in prepared {
            if self.link_cache.enabled { cache_updates.push((height, hash_hex.clone())); }
            owned.push((hash_hex.clone().into_bytes(), value));              // block by hash
            owned.push((height_key(height), hash_hex.clone().into_bytes())); // height index
            if height >= max_h { max_h = height; max_hash = hash_hex; }
        }
        owned.sort_by(|a, b| a.0.cmp(&b.0));
        // LANE 1 atomic SST ingest: off-WAL sorted-SST build + atomic install (rename+fsync),
        // skipping WAL+memtable+compaction. Atomic (ingest-or-nothing); the durable tip advance
        // stays in commit_bulk_trusted_durable AFTER this returns -> kill-9 safe. On ingest error,
        // fall back to the proven batch_put bulk path (zero regression; atomic ingest => no partial).
        match self.db.ingest_sorted_bodies(&owned) {
            Ok(()) => {
                self.best_height = max_h;
                self.best_hash_hex = max_hash;
                self.persist_best();
                for (h, hh) in cache_updates { self.link_cache.record(h, &hh); }
                n
            }
            Err(e) => {
                crate::tlog!("[store] SST ingest failed ({e}) - batch_put fallback ({} blocks)", headers.len());
                self.put_blocks_bulk_trusted(headers)
            }
        }
    }

    /// Advance the contiguous pointer over consecutive heights now present (one pass).
    pub fn advance(&mut self) { self.advance_synced(); }

    // ── v3 (LANE-C): durable batched commit + bulk-load controls ─────────────────
    //
    // The sync-sprint commit path. `put_blocks_batch` lands a chunk in the memtable +
    // WAL under one lock (no fsync, no compaction); these wrap it with explicit,
    // power-loss-safe durability and the bulk-load levers, so a write-back ring can
    // flush LARGE batches with a controlled fsync cadence instead of paying flux-db's
    // implicit 64 MB auto-flush + synchronous compaction storm on the hot path.

    /// v3 (LANE-C): enter/exit bulk-load mode. In bulk-load the underlying flux-db DEFERS
    /// the synchronous post-flush compaction (the >20k-blk/s wall — a forward sync is
    /// append-mostly, so leveled compaction mid-sync is near-pure write-amplification) and
    /// flushes the memtable to SSTs far less often (bigger memtable). Call `compact_to_tip`
    /// once the sync reaches the chain tip to fold the piled-up L0 SSTs down.
    pub fn set_bulk_load(&self, on: bool) {
        self.db.set_defer_compaction(on);
        // Bulk-load grows the memtable so flux-db flushes to SSTs ~4× less often during the
        // sync (each flush clones the memtable + lz4s it); restore 64 MiB for steady-state.
        self.db.set_max_wal_bytes(if on { 256 * 1024 * 1024 } else { 64 * 1024 * 1024 });
    }

    /// v3 (LANE-C): fold the deferred L0 SST pile down — call ONCE at tip (not on the hot
    /// path). Re-enables eager compaction first so steady-state live operation is normal.
    pub fn compact_to_tip(&self) -> Result<(), String> {
        self.db.set_defer_compaction(false);
        self.db.compact()
    }

    /// v3 (LANE-C): fsync only the WAL — the cheap "one fsync per batch" durability point.
    pub fn sync_wal(&self) -> Result<(), String> { self.db.sync_wal() }

    /// v3 (LANE-C): durable batched commit — the seam where verified blocks become durable.
    ///
    /// Stores `headers` (parallel hash+serialize, ONE `batch_put`), then performs the
    /// **2-phase atomic-tip** fsync (design reviewed by DeepSeek — page-cache writeback can
    /// reorder, so a single fsync after a tip-last batch is only kill-9-safe, NOT power-safe):
    ///   1. `put_blocks_batch` → block-by-hash + height-index + best('B') into WAL.
    ///   2. `sync_wal` → those bytes are now durable on disk.
    ///   3. `advance_synced` → writes the contiguous tip watermark 'S' into the WAL.
    ///   4. `sync_wal` → the tip is durable AFTER the blocks it points at.
    /// A crash before (3) loses only the progress marker (re-derived on open by re-scanning
    /// present heights — `open` calls `advance_synced`); a crash between (3) and (4) cannot
    /// persist a tip ahead of its blocks because (2) already forced the blocks durable. So
    /// `synced_to` after recovery is NEVER greater than the set of blocks actually on disk.
    /// `fsync=false` skips both syncs (kill-9-safe via the OS page cache, but not power-safe)
    /// for throughput benchmarking / ephemeral runs. Returns blocks accepted.
    pub fn commit_batch_durable(&mut self, headers: &[SigilBlockHeaderV0], fsync: bool) -> Result<usize, String> {
        if headers.is_empty() { return Ok(0); }
        let accepted = self.put_blocks_batch(headers); // phase 1: blocks+index+best → WAL
        if fsync { self.db.sync_wal()?; }              // phase 2: blocks durable
        self.advance_synced();                         // phase 3: tip 'S' → WAL
        if fsync { self.db.sync_wal()?; }              // phase 4: tip durable AFTER blocks
        Ok(accepted)
    }

    /// v3 (LANE-C, for LANE-B's fold fast-path): durable `verified_to` watermark write — the
    /// crash-safe "set verified_to=C" B needs so a kill-9 between fold-accept and the frontier
    /// commit can NEVER leave a verified watermark ahead of durably-anchored state (the
    /// divergence=0 guarantee at the persistence layer). Same 2-phase fsync as
    /// `commit_batch_durable`: (1) `sync_wal` forces the anchored blocks durable, (2) write the
    /// 'V' watermark, (3) `sync_wal` forces 'V' durable AFTER its anchor. A crash before this
    /// returns simply leaves `verified_to` un-advanced (re-verified on restart) — never ahead.
    ///
    /// `set_verified_to` clamps to `synced_to` (verified ≤ downloaded), which holds today. When
    /// the fold fast-path lands and legitimately verifies a prefix WITHOUT downloading every
    /// block, that clamp must be relaxed UNDER AN EXPLICIT FOLD ANCHOR — owned jointly with
    /// LANE-B; do NOT relax it here unconditionally or a phantom watermark can outlive its data.
    pub fn commit_verified_to_durable(&mut self, h: u64, fsync: bool) -> Result<(), String> {
        if fsync { self.db.sync_wal()?; } // anchored blocks durable BEFORE the watermark
        self.set_verified_to(h);          // writes meta 'V' (clamped to synced_to ∨ fold anchor)
        if fsync { self.db.sync_wal()?; } // watermark durable AFTER its anchor
        Ok(())
    }

    /// v3 (LANE-C, for LANE-B's fold fast-path — Option (b) per swarm msg 395): set the fold
    /// anchor IN-MEMORY (immediately relaxing the `verified_to` clamp to ≤ max(height, synced_to))
    /// and persist it under meta 'A' (no fsync — pair with `commit_fold_anchor_durable` for the
    /// crash-safe path). B calls this BEFORE advancing verified_to to the fold floor, so the
    /// clamp-relax is in effect first.
    pub fn set_fold_anchor(&mut self, height: u64, hash: BlockHash) {
        self.fold_anchor = Some((height, hash));
        let mut v = height.to_be_bytes().to_vec();
        v.extend_from_slice(&hash);
        let _ = self.db.put(&[KEY_META, b'A'], &v);
    }

    /// v3 (LANE-C): DURABLE fold-anchor write — the crash-safe ordering B needs. The anchor is
    /// fsync'd BEFORE any subsequent `commit_verified_to_durable`, so a kill-9 between fold-accept
    /// and the verified_to write can NEVER leave a `verified_to` that exceeds `synced_to` without
    /// a durable anchor to authorize it (which would be a phantom watermark over data we don't
    /// hold). Flow: B calls `commit_fold_anchor_durable(h, hash, true)` → then
    /// `commit_verified_to_durable(h, true)` → then frontier verify.
    pub fn commit_fold_anchor_durable(&mut self, height: u64, hash: BlockHash, fsync: bool) -> Result<(), String> {
        if fsync { self.db.sync_wal()?; } // any prior writes durable first
        self.set_fold_anchor(height, hash); // writes meta 'A' to the WAL
        if fsync { self.db.sync_wal()?; } // anchor durable BEFORE 'V' can reference it
        Ok(())
    }

    pub fn get_block(&self, hash_hex: &str) -> Option<StoredBlock> {
        self.db.get(hash_hex.as_bytes()).ok().flatten().and_then(|v| {
            bincode::deserialize::<StoredBlock>(&v).ok()
        })
    }

    pub fn recent_blocks(&self, n: usize) -> Vec<StoredBlock> {
        let mut blocks: Vec<StoredBlock> = Vec::new();
        let iter = self.db.iter();
        for (_key, value) in iter {
            if let Ok(block) = bincode::deserialize::<StoredBlock>(&value) {
                blocks.push(block);
            }
        }
        blocks.sort_by(|a, b| b.header.height.cmp(&a.header.height));
        blocks.truncate(n);
        blocks
    }

    /// Store a block with just height + hash (light client — no full header needed).
    /// Test-only: insert a fully-formed StoredBlock bypassing ALL ingest guards (height-index
    /// conflict + the v0.95 strict downward-linkage check). Strict ingest makes a parent-broken
    /// block UNSTORABLE through any production path, so verifier-corruption tests — which must
    /// place a deliberately-broken block in storage to prove `verify_to` still catches it as a
    /// second line of defense — use this raw insert instead of `put_block_fast`.
    #[cfg(test)]
    pub(crate) fn force_insert_block(&mut self, header: SigilBlockHeaderV0) {
        let hash_hex = hex::encode(header.hash());
        let height = header.height;
        let block = StoredBlock { header, hash_hex: hash_hex.clone(), synced_at: 0 };
        let value = bincode::serialize(&block).expect("serialize");
        self.db.put(hash_hex.as_bytes(), &value).expect("put block");
        self.db.put(&height_key(height), hash_hex.as_bytes()).expect("put index");
        if height >= self.best_height {
            self.best_height = height;
            self.best_hash_hex = hash_hex;
        }
    }

    pub fn put_block_raw(&mut self, height: u64, hash_hex: &str) -> Result<bool, String> {
        if let Some(existing) = self.height_index_conflict(height, hash_hex) {
            crate::tlog!(
                "[store] rejected raw height-index fork overwrite at h={height}: existing={} incoming={}",
                short_hash_hex(&existing),
                short_hash_hex(hash_hex)
            );
            return Ok(false);
        }
        if self.db.get(hash_hex.as_bytes())?.is_some() {
            return Ok(false);
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let json = format!(r#"{{"h":{},"hash":"{}","ts":{}}}"#, height, hash_hex, ts);
        self.db.put(hash_hex.as_bytes(), json.as_bytes())?;
        self.db.put(&height_key(height), hash_hex.as_bytes())?; // height index
        if height >= self.best_height {
            self.best_height = height;
            self.best_hash_hex = hash_hex.to_string();
            self.persist_best(); // v0.35: keep open() O(1)
        }
        self.advance_synced();
        Ok(true)
    }

    pub fn best_height(&self) -> u64 { self.best_height }
    pub fn best_hash_hex(&self) -> String { self.best_hash_hex.clone() }
    pub fn count(&self) -> usize {
        let mut c = 0usize;
        for (key, _) in self.db.iter() {
            if matches!(key.first(), Some(&KEY_HINDEX) | Some(&KEY_META)) { continue; }
            c += 1;
        }
        c
    }

    pub fn flush(&self) -> Result<(), String> { self.db.flush() }

    /// v7.0.22: flush WITHOUT holding the calling thread. The engine's verify tick called
    /// flush() inline — at 10k blk/s that's a fat memtable→SST write (1-4s measured) on the
    /// apply loop every tick, the operator-visible periodic "rate 0". Same flush(), same
    /// durability + fsync semantics — only the CALLING thread changes. Single-flight: a
    /// flush already in progress makes this a no-op (the next tick retries).
    pub fn flush_background(&self) {
        if !self.db.flush_background() {
            let _ = self.db.flush(); // OS refused a thread — old inline behavior
        }
    }

    pub fn summary(&self) -> BlockStoreSummary {
        BlockStoreSummary {
            total_blocks: self.count(),
            best_height: self.best_height,
            best_hash_hex: self.best_hash_hex.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockStoreSummary {
    pub total_blocks: usize,
    pub best_height: u64,
    pub best_hash_hex: String,
}

// ── v0.11.0: read-only query view for the embedded explorer API ──────────────
//
// `BlockReader` clones the underlying `flux_db::Database` (which is `Arc<RwLock<…>>`
// internally), so it shares the SAME memtable + SSTs + block cache as the live
// `BlockStore` owned by the P2P sync thread — it sees writes as they land, with no
// second open and no lock duplication. This is what lets `serve.rs` answer the
// explorer's `/api/v1/{recent,search,aether}` from the LOCAL verified spine instead
// of blindly proxying every request to the remote sigil-rpcd.

/// One row of the explorer's block/tx feed, sourced from the local store.
#[derive(Debug, Clone, Serialize)]
pub struct BlockRow {
    pub h: u64,
    pub hash: String,
    /// short producer tag (hex prefix of the 32-byte ValidatorId), "" if unknown
    pub prod: String,
    /// content address = the block's own hash hex (verify via aether_verify)
    pub cid: String,
    pub tx_count: u32,
    pub verified: bool,
}

/// Read-only handle over the same flux-db. Cheap to clone.
#[derive(Clone)]
pub struct BlockReader {
    db: flux_db::Database,
    /// genesis anchor / verified watermark are passed in per-call from the sync
    /// state (the reader doesn't own them).
    base: u64,
}

impl BlockStore {
    /// v0.11.0: hand a read-only view of THIS store to another thread (the embedded
    /// HTTP server). Shares the live flux-db; sees the sync thread's writes.
    pub fn reader(&self) -> BlockReader {
        BlockReader { db: self.db.clone(), base: self.base }
    }
}

impl BlockReader {
    /// v0.26 hardening #8: flush the shared flux-db (the reader holds a cloned `Database`
    /// = same Arc as the live store), so a SIGTERM handler can durably persist the
    /// synced/verified watermark before exit without owning the mutable store.
    pub fn flush(&self) -> Result<(), String> { self.db.flush() }

    /// The REAL max stored height, read from the persisted `best` meta key (`[KEY_META,'B']`,
    /// written by the live store's `persist_best`). The sync STATE fakes its height to the
    /// network tip in light-monitor mode, so the explorer must anchor `recent_from`/`search`
    /// HERE — the highest block we ACTUALLY hold — not the tip. Anchoring at the faked tip made
    /// the down-walk start millions of blocks above anything stored → empty → the "at 4.5M but
    /// Activity stuck on 'loading chain'" bug. 0 when the store is empty / key absent.
    pub fn best_height(&self) -> u64 {
        self.db.get(&[KEY_META, b'B']).ok().flatten()
            .filter(|v| v.len() >= 8)
            .map(|v| u64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]))
            .unwrap_or(0)
    }

    /// Decode a stored block (either bincode `StoredBlock` or the light-client raw
    /// JSON `{"h","hash","ts"}`) into a `BlockRow`. `verified` is true only when we
    /// hold the full header and `header.hash()` re-derives to the stored hash hex.
    fn row_for_hash(&self, height_hint: Option<u64>, hash_hex: &str) -> Option<BlockRow> {
        let raw = self.db.get(hash_hex.as_bytes()).ok().flatten()?;
        if let Ok(sb) = bincode::deserialize::<StoredBlock>(&raw) {
            let recomputed = hex::encode(sb.header.hash());
            let verified = recomputed == sb.hash_hex;
            let prod = {
                let p = hex::encode(sb.header.producer);
                p.chars().take(6).collect::<String>()
            };
            return Some(BlockRow {
                h: sb.header.height,
                hash: sb.hash_hex,
                prod,
                cid: hash_hex.to_string(),
                tx_count: sb.header.tx_count,
                verified,
            });
        }
        // light-client raw JSON form
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) {
            let h = v.get("h").and_then(|x| x.as_u64()).or(height_hint).unwrap_or(0);
            let hash = v.get("hash").and_then(|x| x.as_str()).unwrap_or(hash_hex).to_string();
            return Some(BlockRow { h, hash, prod: String::new(), cid: hash_hex.to_string(), tx_count: 0, verified: false });
        }
        None
    }

    fn hash_at_height(&self, h: u64) -> Option<String> {
        let hashk = self.db.get(&height_key(h)).ok().flatten()?;
        String::from_utf8(hashk).ok()
    }

    /// Row at a specific height (via the height index → hash → block).
    pub fn row_at_height(&self, h: u64) -> Option<BlockRow> {
        let hash_hex = self.hash_at_height(h)?;
        self.row_for_hash(Some(h), &hash_hex)
    }

    /// The `n` most-recent blocks at or below `top`, newest first. Walks the height
    /// index DOWN by point lookups (O(n)) — never a full-store scan.
    pub fn recent_from(&self, top: u64, n: usize) -> Vec<BlockRow> {
        let mut out = Vec::with_capacity(n);
        let mut h = top;
        let mut budget = n.saturating_mul(4).max(8);
        loop {
            if out.len() >= n || budget == 0 { break; }
            if let Some(row) = self.row_at_height(h) { out.push(row); }
            budget -= 1;
            if h <= self.base { break; }
            h -= 1;
        }
        out
    }

    /// Local search: numeric query → block at that height; hex query → exact hash
    /// hit, then a bounded scan of recent blocks for a hash-prefix match. Returns []
    /// when nothing local matches so `serve.rs` can fall through to the remote node
    /// (which also indexes txs / full-text). `top` anchors the recent-window scan.
    pub fn search(&self, q: &str, top: u64) -> Vec<BlockRow> {
        let q = q.trim();
        let mut out = Vec::new();
        // 1. exact height
        if let Ok(h) = q.parse::<u64>() {
            if let Some(r) = self.row_at_height(h) { out.push(r); }
        }
        // 2. exact / prefix hash (hex only)
        let is_hex = !q.is_empty() && q.chars().all(|c| c.is_ascii_hexdigit());
        if is_hex {
            if q.len() == 64 {
                if let Some(r) = self.row_for_hash(None, q) {
                    if !out.iter().any(|x: &BlockRow| x.hash == r.hash) { out.push(r); }
                }
            }
            if out.len() < 12 {
                let ql = q.to_ascii_lowercase();
                let mut h = top;
                let mut budget = 400usize;
                while out.len() < 12 && budget > 0 {
                    if let Some(r) = self.row_at_height(h) {
                        if r.hash.to_ascii_lowercase().contains(&ql)
                            && !out.iter().any(|x: &BlockRow| x.hash == r.hash) {
                            out.push(r);
                        }
                    }
                    budget -= 1;
                    if h <= self.base { break; }
                    h -= 1;
                }
            }
        }
        out
    }

    /// flux-aether content-address verify (LOCAL, verify-don't-trust): look the cid up
    /// as a block hash, re-derive `header.hash()`, and report whether it matches. Returns
    /// None when the cid isn't a local block (→ serve.rs proxies to the remote aether).
    pub fn aether_verify(&self, cid: &str) -> Option<BlockRow> {
        self.row_for_hash(None, cid)
    }
}

// ── v0.7.1: Streaming aether sync ──

/// Sync blocks from sigil-node's aether shards into our flux-db BlockStore.
/// Reads shards sequentially, finds valid JSON blocks via `},{` separators,
/// and stores them one at a time into flux-db.
pub fn sync_aether_to_fluxdb(store: &mut BlockStore, aether_dir: &str) -> Result<usize, String> {
    let dir = std::path::Path::new(aether_dir);
    if !dir.exists() {
        return Err("aether dir not found".into());
    }
    let manifest_raw = std::fs::read_to_string(dir.join("manifest"))
        .map_err(|e| format!("read manifest: {e}"))?;
    let parts: Vec<&str> = manifest_raw.split_whitespace().collect();
    let total_bytes: usize = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let shard_count: usize = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let best_height: u64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);

    eprintln!("[aether] chain={best_height} bytes={total_bytes} shards={shard_count} stored={}", store.best_height());
    if best_height <= store.best_height() {
        return Ok(0);
    }

    // Concatenate shards up to total_bytes
    let mut buf = Vec::with_capacity(total_bytes.min(200_000_000));
    for i in 0..shard_count {
        let path = dir.join(format!("shard.{i}"));
        let shard = std::fs::read(&path)
            .map_err(|e| format!("read shard.{i}: {e}"))?;
        buf.extend_from_slice(&shard);
        if buf.len() >= total_bytes {
            buf.truncate(total_bytes);
            break;
        }
    }
    buf.truncate(total_bytes.min(buf.len()));

    // Find last ']' for clean array close
    if let Some(last_br) = buf.iter().rposition(|&b| b == b']') {
        buf.truncate(last_br + 1);
    }
    // If data doesn't start with '[', wrap it
    if !buf.starts_with(b"[") {
        buf.insert(0, b'[');
    }

    let json_str = String::from_utf8_lossy(&buf).into_owned();
    eprintln!("[aether] parsing {} bytes...", json_str.len());

    let raw_blocks: Vec<serde_json::Value> = serde_json::from_str(&json_str)
        .map_err(|e| format!("parse {}B: {e}", json_str.len()))?;

    eprintln!("[aether] parsed {} blocks total", raw_blocks.len());

    let start = store.best_height() + 1;
    let mut synced = 0usize;
    for raw in &raw_blocks {
        let hdr = &raw["header"];
        let height = hdr["height"].as_u64().unwrap_or(0);
        if height < start { continue; }
        if height > best_height { break; }
        let hash_hex = raw.get("hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("aether-h-{height}"));
        if store.put_block_raw(height, &hash_hex).unwrap_or(false) {
            synced += 1;
        }
    }

    store.flush()?;
    eprintln!("[aether] → flux-db: {synced} new blocks, height {}", store.best_height());
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-bstore-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    /// Wire-codec lab tool (env-gated, skips by default): open the REAL local block DB
    /// (`SIGIL_BENCH_DB=/path`) and dump an exact `'H' + bincode(Vec<Header>)` backfill
    /// payload — byte-identical to what `headers_only` puts on the wire — to
    /// `/tmp/wire-chunk-real.bin`. Compression candidates (lz4 vs zstd) are then benched
    /// on REAL header bytes, not synthetic test headers whose empty proofs over-compress.
    #[test]
    fn dump_real_wire_chunk_for_codec_bench() {
        let db = match std::env::var("SIGIL_BENCH_DB") { Ok(p) => p, Err(_) => return };
        let s = match BlockStore::open(&db) { Ok(s) => s, Err(e) => { eprintln!("[dump] open: {e}"); return } };
        eprintln!("[dump] db={db} best_height={} synced_to={}", s.best_height(), s.synced_to());
        // ONE sequential iter scan — cold point-reads on a 2.7G LSM are ~ms each (the
        // first two versions of this dump hung for minutes probing a gappy height index).
        // A codec bench just needs 4096 REAL header byte-streams; order doesn't matter.
        let mut headers: Vec<SigilBlockHeaderV0> = Vec::with_capacity(4096);
        for (key, value) in s.db.iter() {
            if matches!(key.first(), Some(&KEY_HINDEX) | Some(&KEY_META)) { continue; }
            if let Ok(block) = bincode::deserialize::<StoredBlock>(&value) {
                headers.push(block.header);
                if headers.len() >= 4096 { break; }
            }
        }
        eprintln!("[dump] collected {} real headers via iter scan", headers.len());
        if headers.is_empty() { eprintln!("[dump] no headers found"); return; }
        headers.sort_by_key(|h| h.height);
        let mut payload = vec![b'H'];
        payload.extend(bincode::serialize(&headers).expect("serialize"));
        std::fs::write("/tmp/wire-chunk-real.bin", &payload).expect("write");
        eprintln!("[dump] wrote /tmp/wire-chunk-real.bin: {} headers, {} bytes ({:.0} B/header)",
            headers.len(), payload.len(), payload.len() as f64 / headers.len() as f64);
    }

    // Catches the monitor-sync class of bug (synced_to not advancing / not resuming)
    // that previously only surfaced on slow live runs.
    #[test]
    fn synced_to_advances_contiguously_stops_at_gaps_and_resumes() {
        let p = tmp("sync");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut s = BlockStore::open(&p).unwrap();
            assert_eq!(s.synced_to(), 0, "fresh store starts at 0");
            for h in 0..5 {
                assert!(s.put_block_raw(h, &format!("hash{h}")).unwrap());
            }
            assert_eq!(s.synced_to(), 5, "0..4 present -> next needed = 5");
            s.put_block_raw(7, "hash7").unwrap(); // gap at 5,6
            assert_eq!(s.synced_to(), 5, "a gap holds the contiguous pointer");
            s.put_block_raw(5, "hash5").unwrap();
            s.put_block_raw(6, "hash6").unwrap();
            assert_eq!(s.synced_to(), 8, "filling the gap jumps the pointer to 8");
            assert_eq!(s.count(), 8, "8 distinct blocks stored");
        }
        let s2 = BlockStore::open(&p).unwrap();
        assert_eq!(s2.synced_to(), 8, "RESUMES from the persisted store, not 0");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// v0.95 REGRESSION: with base=1 (SIGIL's genesis-anchor, h=0 not backfill-servable) a
    /// v7.0.18: a poisoned frontier seam (a bulk-lane block whose hash doesn't match the
    /// honest chain — `put_blocks_bulk_trusted` does no linkage checks) wedges ingest
    /// forever: every honest chunk rejects (index fork at the seam, parent-mismatch above).
    /// `rollback_frontier` + refetch must fully heal it — including re-creating the height
    /// index for blocks whose BODIES survived the rollback.
    #[test]
    fn rollback_frontier_heals_poisoned_seam() {
        use sigil_header::*;
        fn mk(height: u64, parent: BlockHash) -> SigilBlockHeaderV0 {
            let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
            let mut hh = blake3::Hasher::new();
            hh.update(&parent); hh.update(nonce.as_bytes());
            let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
            let scheme = SigScheme::SqiSign5;
            SigilBlockHeaderV0 {
                version: HEADER_VERSION, network_id: NETWORK_ID, height, parent_hash: parent,
                merge_parents: Vec::new(), timestamp_ms: 1000 + height, nonce_sqisign: nonce,
                vdf_input, vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 }, difficulty: 1,
                wallet_state_root: [0u8;32], dex_state_root: [0u8;32], event_log_root: [0u8;32],
                contract_state_root: [0u8;32],
                state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8;32] },
                txs_merkle_root: [0u8;32], tx_count: 0,
                fluxc_artifact_proof: ProofBundle { artifact_blake3: [0u8;32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None },
                sig_scheme: scheme, producer: [0u8;32],
                producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
                topology_commitment: None,
            }
        }
        let p = tmp("rollback-heal");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut s = BlockStore::open(&p).unwrap();
            s.set_base(1);
            // Honest chain h=0..19.
            let mut chain = Vec::new();
            let mut parent = [0u8; 32];
            for h in 0..20u64 { let hdr = mk(h, parent); parent = hdr.hash(); chain.push(hdr); }
            assert_eq!(s.put_blocks_batch(&chain[1..10]), 9);
            s.advance();
            assert_eq!(s.synced_to(), 10, "honest [1..9] stored");
            // POISON the seam: bulk-style unlinked block at h=10 (garbage parent).
            s.force_insert_block(mk(10, [0xAA; 32]));
            s.advance();
            assert_eq!(s.synced_to(), 11, "poison occupies h=10");
            // Honest chunk [10..19] fully rejects against the poisoned seam → wedge.
            let got = s.put_blocks_batch(&chain[10..20]);
            s.advance();
            assert_eq!(got, 0, "honest chunk rejected (index fork at 10, parent-mismatch above)");
            assert_eq!(s.synced_to(), 11, "wedged");
            // SELF-HEAL: roll back past the seam, then refetch clean.
            let newf = s.rollback_frontier(4);
            assert_eq!(newf, 7, "frontier rolled back below the seam");
            assert_eq!(s.synced_to(), 7);
            let got2 = s.put_blocks_batch(&chain[7..20]);
            s.advance();
            assert!(got2 >= 10, "refetched chunk accepted after heal (got {got2})");
            assert_eq!(s.synced_to(), 20, "healed — frontier passes the poisoned seam");
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    /// frontier chunk [1..N] under strict downward-linkage must store + advance — h=1 is the
    /// exempt anchor (its parent h=0 is never served), h=2.. link upward. Guards against the
    /// strict-ingest change self-stalling the live anchor (synced parked at 1).
    #[test]
    fn base1_anchor_chunk_stores_and_advances_under_strict_ingest() {
        use sigil_header::*;
        fn mk(height: u64, parent: BlockHash) -> SigilBlockHeaderV0 {
            let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
            let mut hh = blake3::Hasher::new();
            hh.update(&parent); hh.update(nonce.as_bytes());
            let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
            let scheme = SigScheme::SqiSign5;
            SigilBlockHeaderV0 {
                version: HEADER_VERSION, network_id: NETWORK_ID, height, parent_hash: parent,
                merge_parents: Vec::new(), timestamp_ms: 1000 + height, nonce_sqisign: nonce,
                vdf_input, vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 }, difficulty: 1,
                wallet_state_root: [0u8;32], dex_state_root: [0u8;32], event_log_root: [0u8;32],
                contract_state_root: [0u8;32],
                state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8;32] },
                txs_merkle_root: [0u8;32], tx_count: 0,
                fluxc_artifact_proof: ProofBundle { artifact_blake3: [0u8;32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None },
                sig_scheme: scheme, producer: [0u8;32],
                producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
                topology_commitment: None,
            }
        }
        let p = tmp("base1-anchor");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut s = BlockStore::open(&p).unwrap();
            s.set_base(1);
            assert_eq!(s.synced_to(), 1, "anchor base=1");
            // Build h=0..9 linking; the mesh serves [1..9] (h=0 genesis never served).
            let mut chain = Vec::new();
            let mut parent = [0u8; 32];
            for h in 0..10u64 { let hdr = mk(h, parent); parent = hdr.hash(); chain.push(hdr); }
            let stored = s.put_blocks_batch(&chain[1..10]); // [1..9], parent h=0 absent
            s.advance();
            assert_eq!(stored, 9, "all 9 of [1..9] accept (h=1 anchor exempt, rest link)");
            assert_eq!(s.synced_to(), 10, "frontier advances to 10, NOT parked at the anchor");
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn height_index_rejects_conflicting_raw_overwrite() {
        let p = tmp("height-conflict-raw");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut s = BlockStore::open(&p).unwrap();
            assert!(s.put_block_raw(8193, "canonical8193").unwrap());
            assert!(!s.put_block_raw(8193, "fork8193").unwrap());
            assert_eq!(
                s.hash_at_index_height(8193).as_deref(),
                Some("canonical8193"),
                "a second hash for the same height must not replace the spine index"
            );
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    /// LANE-S acceptance (mechanism): a stale client (OLD-genesis store with high watermarks) that
    /// meets a FRESH-genesis producer must AUTO-WIPE its watermarks via `note_genesis` — the
    /// self-heal that replaces the manual local wipe after a testnet restart. The live <10 s gate
    /// is an integration concern; this CI test proves the keying: detect the changed genesis
    /// anchor → zero + persist the watermarks, and never re-trigger on the same anchor.
    #[test]
    fn lane_s_genesis_change_auto_invalidates_watermarks() {
        let p = tmp("lane-s-genesis");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut s = BlockStore::open(&p).unwrap();
            // OLD chain: learn its genesis anchor, then make progress.
            assert!(!s.note_genesis("old_genesis_hash"), "first genesis is LEARNED, not a reset");
            assert_eq!(s.genesis_hash(), "old_genesis_hash");
            for h in 0..6 { s.put_block_raw(h, &format!("old{h}")).unwrap(); }
            s.set_verified_to(5);
            assert_eq!(s.synced_to(), 6);
            assert_eq!(s.verified_to(), 5);
            assert_eq!(s.best_height(), 5);
            // Fresh genesis (testnet restart) → DIFFERENT anchor → AUTO-INVALIDATE.
            assert!(s.note_genesis("fresh_genesis_hash"), "a CHANGED genesis is a reset");
            assert_eq!(s.genesis_hash(), "fresh_genesis_hash", "adopts the fresh anchor");
            assert_eq!(s.synced_to(), 0, "stale watermarks wiped on genesis change");
            assert_eq!(s.verified_to(), 0);
            assert_eq!(s.best_height(), 0);
            // Idempotent: the SAME genesis again is NOT a reset.
            assert!(!s.note_genesis("fresh_genesis_hash"));
            // Unknown (empty) live genesis is a no-op (offline / pre-anchor).
            assert!(!s.note_genesis(""));
            assert_eq!(s.genesis_hash(), "fresh_genesis_hash");
        }
        // The fresh anchor PERSISTS across reopen (keyed under meta 'G').
        {
            let s = BlockStore::open(&p).unwrap();
            assert_eq!(s.genesis_hash(), "fresh_genesis_hash", "genesis key survives reopen");
        }
        let _ = std::fs::remove_dir_all(&p);
    }
}
