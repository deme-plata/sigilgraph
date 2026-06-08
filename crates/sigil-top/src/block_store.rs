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
}

impl BlockStore {
    pub fn open(path: &str) -> Result<Self, String> {
        let db_path = PathBuf::from(path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
        }
        let db = flux_db::Database::open(&db_path)?;

        let mut best_height = 0u64;
        let mut best_hash_hex = String::new();
        let iter = db.iter();
        for (_key, value) in iter {
            if let Ok(block) = bincode::deserialize::<StoredBlock>(&value) {
                if block.header.height > best_height {
                    best_height = block.header.height;
                    best_hash_hex = block.hash_hex.clone();
                }
            }
        }

        Ok(BlockStore { db, best_height, best_hash_hex })
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

        let value = bincode::serialize(&block).map_err(|e| format!("serialize: {}", e))?;
        self.db.put(hash_hex.as_bytes(), &value)?;

        if block.header.height > self.best_height {
            self.best_height = block.header.height;
            self.best_hash_hex = hash_hex;
        }

        Ok(true)
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
        if height > self.best_height {
            self.best_height = height;
            self.best_hash_hex = hash_hex.to_string();
        }
        Ok(true)
    }

    pub fn best_height(&self) -> u64 { self.best_height }
    pub fn best_hash_hex(&self) -> String { self.best_hash_hex.clone() }
    pub fn count(&self) -> usize {
        let mut c = 0usize;
        let iter = self.db.iter();
        for _ in iter { c += 1; }
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
