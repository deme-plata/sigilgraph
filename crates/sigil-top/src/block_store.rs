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
}

/// Key prefix bytes (block data is keyed by 64-char hex hash, never starts with these).
const KEY_HINDEX: u8 = 0x01; // 0x01 ++ height.to_be_bytes() -> hash_hex  (height index)
const KEY_META: u8 = 0x02;   // 0x02'S' -> synced_to · 0x02'V' -> verified_to  (meta)

fn height_key(h: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(KEY_HINDEX);
    k.extend_from_slice(&h.to_be_bytes());
    k
}

impl BlockStore {
    pub fn open(path: &str) -> Result<Self, String> {
        let db_path = PathBuf::from(path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }
        let mut db = flux_db::Database::open(&db_path)?;

        let mut best_height = 0u64;
        let mut best_hash_hex = String::new();
        let mut have_index = false;
        let mut blocks_idx: Vec<(u64, Vec<u8>)> = Vec::new(); // (height, hash bytes) for migration
        for (key, value) in db.iter() {
            match key.first() {
                Some(&KEY_HINDEX) => { have_index = true; continue; }
                Some(&KEY_META) => continue,
                _ => {}
            }
            if let Ok(block) = bincode::deserialize::<StoredBlock>(&value) {
                if block.header.height >= best_height {
                    best_height = block.header.height;
                    best_hash_hex = block.hash_hex.clone();
                }
                blocks_idx.push((block.header.height, key));
            }
        }
        // Migrate a pre-index store (built before the height index existed) so it
        // resumes instead of re-syncing from 0.
        if !have_index && !blocks_idx.is_empty() {
            for (h, hashk) in &blocks_idx {
                let _ = db.put(&height_key(*h), hashk);
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

        let mut s = BlockStore { db, best_height, best_hash_hex, synced_to, verified_to, base: 0 };
        s.advance_synced(); // catch up the contiguous pointer to whatever's on disk
        Ok(s)
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

    /// v0.9.0: Load the full stored header at a given height (via the height index →
    /// hash → block). Returns None if that height isn't stored. This is the read path the
    /// chain verifier walks to recompute `hash()` and check `parent_hash` linkage.
    pub fn get_header_at_height(&self, height: u64) -> Option<SigilBlockHeaderV0> {
        let hashk = self.db.get(&height_key(height)).ok().flatten()?;
        let hash_hex = String::from_utf8(hashk).ok()?;
        self.get_block(&hash_hex).map(|b| b.header)
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
        // Build all (key, value) byte pairs first; batch_put borrows them in one shot.
        let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(headers.len() * 2);
        let mut max_h = self.best_height;
        let mut max_hash = self.best_hash_hex.clone();
        for header in headers {
            let hash_hex = hex::encode(header.hash());
            let height = header.height;
            let block = StoredBlock { header: header.clone(), hash_hex: hash_hex.clone(), synced_at: 0 };
            let value = match bincode::serialize(&block) { Ok(v) => v, Err(_) => continue };
            owned.push((hash_hex.clone().into_bytes(), value));               // block by hash
            owned.push((height_key(height), hash_hex.clone().into_bytes()));  // height index
            if height >= max_h { max_h = height; max_hash = hash_hex; }
        }
        let refs: Vec<(&[u8], &[u8])> = owned.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
        match self.db.batch_put(&refs) {
            Ok(()) => { self.best_height = max_h; self.best_hash_hex = max_hash; headers.len() }
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
}
