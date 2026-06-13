// sigil-top/src/block_store.rs — Local block storage via flux-db (v0.7.1)
//
// Persists SIGIL block headers to flux-db, maintains chain continuity,
// and provides query methods for the dashboard.
//
// v0.7.1: Added streaming aether sync — reads sigil-node shards directly
// into flux-db, one block at a time via serde_json.

use serde::{Deserialize, Serialize};
use sigil_header::SigilBlockHeaderV0;
use std::path::PathBuf;

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
}

/// Key prefix bytes (block data is keyed by 64-char hex hash, never starts with these).
const KEY_HINDEX: u8 = 0x01; // 0x01 ++ height.to_be_bytes() -> hash_hex  (height index)
const KEY_META: u8 = 0x02;   // 0x02'S' -> synced_to · 0x02'V' -> verified_to  (meta)
                             // 0x02'B' -> be(best_height) ++ best_hash_hex  (v0.35: O(1) open)
                             // 0x02'G' -> genesis-anchor hash hex            (LANE-S: reset key)

fn height_key(h: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(KEY_HINDEX);
    k.extend_from_slice(&h.to_be_bytes());
    k
}

impl BlockStore {
    /// Open for the interactive TUI: returns IMMEDIATELY. A store that predates the
    /// 'B' fast-open key migrates its index on a BACKGROUND thread (see
    /// [`migrate_index`](Self::migrate_index)) so the dashboard paints at once
    /// instead of hanging on a multi-GB scan. Under `cfg!(test)` migration is inline
    /// so tests observe `best_height` synchronously.
    pub fn open(path: &str) -> Result<Self, String> {
        Self::open_inner(path, false)
    }

    /// Open for headless tooling (verify / full-sync / explorer serve) that needs
    /// `best_height` + the height index on return: the migration runs INLINE.
    pub fn open_blocking(path: &str) -> Result<Self, String> {
        Self::open_inner(path, true)
    }

    fn open_inner(path: &str, inline_migration: bool) -> Result<Self, String> {
        let db_path = PathBuf::from(path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }
        let db = flux_db::Database::open(&db_path)?;

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
        let verified_to = db
            .get(&[KEY_META, b'V']).ok().flatten()
            .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok().map(u64::from_be_bytes))
            .unwrap_or(0)
            .min(synced_to); // never claim verified past what's downloaded

        // LANE-S: the genesis-anchor hash the persisted watermarks belong to.
        let genesis_hash = db
            .get(&[KEY_META, b'G']).ok().flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let mut s = BlockStore { db, best_height, best_hash_hex, synced_to, verified_to, base: 0, genesis_hash };
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
        self.advance_synced();
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
        let h = h.min(self.synced_to);
        if h != self.verified_to {
            self.verified_to = h;
            let _ = self.db.put(&[KEY_META, b'V'], &self.verified_to.to_be_bytes());
        }
    }

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
        let _ = self.db.put(&[KEY_META, b'S'], &0u64.to_be_bytes());
        let _ = self.db.put(&[KEY_META, b'V'], &0u64.to_be_bytes());
        let _ = self.db.put(&[KEY_META, b'G'], b""); // clear the persisted genesis key
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

        if self.db.get(hash_hex.as_bytes())?.is_some() {
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

        let height = block.header.height;
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
        let prepared: Vec<(u64, String, Vec<u8>)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(nthreads);
            for chunk in headers.chunks(chunk_sz) {
                handles.push(s.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for header in chunk {
                        let hash_hex = hex::encode(header.hash());
                        let block = StoredBlock {
                            header: header.clone(), hash_hex: hash_hex.clone(), synced_at: 0,
                        };
                        if let Ok(value) = bincode::serialize(&block) {
                            out.push((block.header.height, hash_hex, value));
                        }
                    }
                    out
                }));
            }
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        // Serial tail: assemble the (key, value) pairs + ONE batch_put (single WAL hold).
        let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(prepared.len() * 2);
        let mut max_h = self.best_height;
        let mut max_hash = self.best_hash_hex.clone();
        for (height, hash_hex, value) in prepared {
            owned.push((hash_hex.clone().into_bytes(), value));              // block by hash
            owned.push((height_key(height), hash_hex.clone().into_bytes())); // height index
            if height >= max_h { max_h = height; max_hash = hash_hex; }
        }
        let refs: Vec<(&[u8], &[u8])> = owned.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
        match self.db.batch_put(&refs) {
            Ok(()) => {
                self.best_height = max_h;
                self.best_hash_hex = max_hash;
                self.persist_best(); // v0.35: ONE meta put per batch keeps open() O(1)
                headers.len()
            }
            Err(_) => 0,
        }
    }

    /// Advance the contiguous pointer over consecutive heights now present (one pass).
    pub fn advance(&mut self) { self.advance_synced(); }

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
    pub fn put_block_raw(&mut self, height: u64, hash_hex: &str) -> Result<bool, String> {
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
