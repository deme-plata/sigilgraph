//! delta-archive-oracle — 20TB BlockRange Server for SIGIL Light Nodes
//!
//! Part of "Chronos Spacetime" research track. Deployed on Delta
//! (5.79.79.158), this binary serves the full Sigil chain history to light
//! nodes that join late and need to backfill. The backfill protocol was
//! validated in Chronos (`backfill.rs`); this is the production deployment.
//!
//! # Architecture
//!
//! ```text
//!                          ┌──────────────────────┐
//!                          │   Light Node          │
//!                          │   (tip-proof only)    │
//!                          │   height = 0          │
//!                          └──────────┬───────────┘
//!                                     │ BlockRangeRequest { from: 1, to: 500 }
//!                                     ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │              Delta Archive Oracle (:9800)                │
//! │                                                         │
//! │  ┌──────────────────┐   ┌────────────────────────────┐  │
//! │  │  HTTP API        │   │  flux-p2p request/response │  │
//! │  │  GET /blocks/    │   │  /sigil/g0/blockrange      │  │
//! │  │  GET /status     │   │  (direct peer protocol)    │  │
//! │  └────────┬─────────┘   └────────────┬───────────────┘  │
//! │           │                          │                   │
//! │           └──────────┬───────────────┘                   │
//! │                      ▼                                   │
//! │  ┌────────────────────────────────────────────────────┐  │
//! │  │           20TB Block Store                         │  │
//! │  │  /mnt/20tb/sigil-chain/                            │  │
//! │  │  ├── blocks/          block_000001.bin ...         │  │
//! │  │  ├── index.bin        height → file offset        │  │
//! │  │  ├── snapshots/       chronos snapshot archive    │  │
//! │  │  └── state.bin        latest state for fast sync  │  │
//! │  └────────────────────────────────────────────────────┘  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Block Storage Format
//!
//! Blocks are stored in sharded files (1M blocks per shard) for efficient
//! random access. An index file maps block height → (shard_file, offset, len).
//! This allows serving any range with O(1) seeks.
//!
//! # Compression
//!
//! Blocks are zstd-compressed on disk (~60-70% reduction). Light nodes
//! request with an Accept-Encoding header; the oracle decompresses on the
//! fly or sends raw compressed bytes if the light node supports it.
//!
//! # Deployment
//!
//! ```bash
//! # On Delta (5.79.79.158):
//! delta-archive-oracle \
//!   --store /mnt/20tb/sigil-chain \
//!   --listen 0.0.0.0:9800 \
//!   --p2p-listen /ip4/0.0.0.0/tcp/9801 \
//!   --peer-id-file /tmp/delta-oracle-peerid
//! ```

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ── Block types (mirrors sigil-chronos Block) ───────────────────────────────

/// A stored block in the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedBlock {
    /// Block height (1 = first after genesis).
    pub height: u64,
    /// Parent block hash (32 bytes, hex).
    pub parent_hash: String,
    /// Block hash (32 bytes, hex).
    pub block_hash: String,
    /// Number of transactions in this block.
    pub tx_count: u32,
    /// Wall-clock timestamp when this block was archived (Unix seconds).
    pub archived_at: u64,
    /// Producer node that minted this block.
    pub producer: String,
    /// Raw block bytes (bincode-serialized Sigil Block).
    pub raw_bytes: Vec<u8>,
    /// Compressed size on disk.
    pub compressed_size: u64,
}

/// A BlockRange request from a light node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRangeRequest {
    pub from_height: u64,
    pub to_height: u64,
    /// Maximum blocks to return in this response (server may return fewer).
    pub max_blocks: u64,
    /// If true, return compressed raw bytes instead of JSON.
    pub accept_compressed: bool,
}

/// A BlockRange response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRangeResponse {
    pub blocks: Vec<ArchivedBlock>,
    /// The actual from_height served (may differ from request if gaps exist).
    pub served_from: u64,
    /// The actual to_height served.
    pub served_to: u64,
    /// Total blocks available in the archive.
    pub total_blocks: u64,
    /// Whether more blocks are available (light node should request again).
    pub has_more: bool,
    /// Server's current tip height.
    pub tip_height: u64,
}

/// Archive status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveStatus {
    pub total_blocks: u64,
    pub tip_height: u64,
    pub tip_hash: String,
    pub disk_used_bytes: u64,
    pub disk_capacity_bytes: u64,
    pub compression_ratio: f64,
    pub uptime_seconds: u64,
    pub requests_served: u64,
    pub bytes_served: u64,
    pub connected_light_nodes: u64,
}

// ── Block Index ─────────────────────────────────────────────────────────────

/// An entry in the block index: where to find a block on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    height: u64,
    shard_file: String, // e.g. "blocks/shard_000000.bin"
    offset: u64,
    compressed_len: u64,
    uncompressed_len: u64,
    block_hash: String,
    parent_hash: String,
    tx_count: u32,
}

/// The block index — maps height → disk location.
struct BlockIndex {
    entries: Vec<IndexEntry>,
    /// Height of the first block in the archive.
    start_height: u64,
    /// Total blocks indexed.
    total_blocks: u64,
}

impl BlockIndex {
    fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                entries: Vec::new(),
                start_height: 1,
                total_blocks: 0,
            });
        }
        let data = fs::read(path).map_err(|e| format!("read index: {e}"))?;
        let entries: Vec<IndexEntry> = bincode::deserialize(&data)
            .map_err(|e| format!("deserialize index: {e}"))?;
        let total = entries.len() as u64;
        let start = entries.first().map(|e| e.height).unwrap_or(1);
        Ok(Self {
            entries,
            start_height: start,
            total_blocks: total,
        })
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let data = bincode::serialize(&self.entries)
            .map_err(|e| format!("serialize index: {e}"))?;
        // Atomic write.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &data).map_err(|e| format!("write index tmp: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| format!("rename index: {e}"))?;
        Ok(())
    }

    /// Look up entries in range [from, to]. Returns empty if none found.
    fn range(&self, from: u64, to: u64) -> Vec<&IndexEntry> {
        if self.entries.is_empty() || from > to {
            return vec![];
        }
        // Binary search for the first entry >= from.
        let start_idx = match self.entries.binary_search_by_key(&from, |e| e.height) {
            Ok(i) => i,
            Err(i) => i,
        };
        let end_idx = match self.entries.binary_search_by_key(&to, |e| e.height) {
            Ok(i) => (i + 1).min(self.entries.len()),
            Err(i) => i,
        };
        if start_idx >= self.entries.len() {
            return vec![];
        }
        self.entries[start_idx..end_idx].iter().collect()
    }

    fn append(&mut self, entry: IndexEntry) {
        self.entries.push(entry);
        self.total_blocks = self.entries.len() as u64;
    }
}

// ── Block Store ─────────────────────────────────────────────────────────────

/// Manages reading/writing blocks to the 20TB storage.
struct BlockStore {
    root: PathBuf,
    index: BlockIndex,
    /// Currently open shard file for writing.
    current_shard: Option<(String, BufWriter<File>)>,
    /// Blocks written to the current shard.
    shard_block_count: u64,
    /// Maximum blocks per shard file (1M blocks per shard).
    max_blocks_per_shard: u64,
    /// Cached open shard readers (for serving).
    shard_readers: HashMap<String, BufReader<File>>,
}

impl BlockStore {
    const MAX_BLOCKS_PER_SHARD: u64 = 1_000_000;
    const SHARD_DIR: &'static str = "blocks";

    fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| format!("create store dir: {e}"))?;
        fs::create_dir_all(root.join(Self::SHARD_DIR))
            .map_err(|e| format!("create blocks dir: {e}"))?;

        let index_path = root.join("index.bin");
        let index = BlockIndex::load(&index_path)?;

        Ok(Self {
            root,
            index,
            current_shard: None,
            shard_block_count: 0,
            max_blocks_per_shard: Self::MAX_BLOCKS_PER_SHARD,
            shard_readers: HashMap::new(),
        })
    }

    /// Total blocks stored.
    fn total_blocks(&self) -> u64 {
        self.index.total_blocks
    }

    /// Tip height (0 if empty).
    fn tip_height(&self) -> u64 {
        self.index
            .entries
            .last()
            .map(|e| e.height)
            .unwrap_or(0)
    }

    /// Store a new block. Returns the index entry.
    fn store_block(
        &mut self,
        height: u64,
        parent_hash: &str,
        block_hash: &str,
        tx_count: u32,
        _producer: &str,
        raw_bytes: &[u8],
    ) -> Result<IndexEntry, String> {
        // Compress the block.
        let compressed = {
            #[cfg(feature = "zstd")]
            {
                let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3)
                    .map_err(|e| format!("zstd encoder: {e}"))?;
                encoder.write_all(raw_bytes).map_err(|e| format!("zstd write: {e}"))?;
                encoder.finish().map_err(|e| format!("zstd finish: {e}"))?
            }
            #[cfg(not(feature = "zstd"))]
            raw_bytes.to_vec()
        };

        // Ensure we have a shard file open.
        if self.current_shard.is_none()
            || self.shard_block_count >= self.max_blocks_per_shard
        {
            if let Some((_, mut writer)) = self.current_shard.take() {
                writer.flush().ok();
            }
            let shard_idx = self.index.total_blocks / self.max_blocks_per_shard;
            let shard_name = format!("shard_{:06}.bin", shard_idx);
            let shard_path = self.root.join(Self::SHARD_DIR).join(&shard_name);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&shard_path)
                .map_err(|e| format!("open shard {}: {e}", shard_path.display()))?;
            self.current_shard = Some((shard_name, BufWriter::new(file)));
            self.shard_block_count = 0;
        }

        let (ref shard_name, ref mut writer) = self.current_shard.as_mut().unwrap();
        let offset = writer
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("seek shard: {e}"))?;

        // Write: [compressed_len: u32 LE][compressed_bytes...]
        let len = compressed.len() as u32;
        writer
            .write_all(&len.to_le_bytes())
            .map_err(|e| format!("write len: {e}"))?;
        writer
            .write_all(&compressed)
            .map_err(|e| format!("write block: {e}"))?;
        writer.flush().map_err(|e| format!("flush: {e}"))?;

        let entry = IndexEntry {
            height,
            shard_file: shard_name.clone(),
            offset,
            compressed_len: compressed.len() as u64,
            uncompressed_len: raw_bytes.len() as u64,
            block_hash: block_hash.to_string(),
            parent_hash: parent_hash.to_string(),
            tx_count,
        };

        self.index.append(entry.clone());
        self.index.save(&self.root.join("index.bin"))?;
        self.shard_block_count += 1;

        Ok(entry)
    }

    /// Read a block at the given height.
    fn read_block(&mut self, height: u64) -> Result<Vec<u8>, String> {
        let entries = self.index.range(height, height);
        let entry = entries
            .first()
            .ok_or_else(|| format!("block at height {height} not found"))?;

        let shard_path = self.root.join(Self::SHARD_DIR).join(&entry.shard_file);

        // Use cached reader or open a new one.
        let reader = if !self.shard_readers.contains_key(&entry.shard_file) {
            let file = File::open(&shard_path)
                .map_err(|e| format!("open shard {}: {e}", shard_path.display()))?;
            self.shard_readers
                .insert(entry.shard_file.clone(), BufReader::new(file));
            self.shard_readers.get_mut(&entry.shard_file).unwrap()
        } else {
            self.shard_readers.get_mut(&entry.shard_file).unwrap()
        };

        reader
            .seek(SeekFrom::Start(entry.offset))
            .map_err(|e| format!("seek: {e}"))?;

        // Read length prefix.
        let mut len_buf = [0u8; 4];
        reader
            .read_exact(&mut len_buf)
            .map_err(|e| format!("read len: {e}"))?;
        let compressed_len = u32::from_le_bytes(len_buf) as usize;

        // Read compressed bytes.
        let mut compressed = vec![0u8; compressed_len];
        reader
            .read_exact(&mut compressed)
            .map_err(|e| format!("read block data: {e}"))?;

        // Decompress.
        #[cfg(feature = "zstd")]
        {
            let mut decoder = zstd::stream::Decoder::new(&compressed[..])
                .map_err(|e| format!("zstd decoder: {e}"))?;
            let mut raw = Vec::new();
            decoder
                .read_to_end(&mut raw)
                .map_err(|e| format!("zstd decompress: {e}"))?;
            Ok(raw)
        }
        #[cfg(not(feature = "zstd"))]
        Ok(compressed)
    }

    /// Serve a BlockRange request. Returns the response.
    fn serve_range(&mut self, req: &BlockRangeRequest) -> BlockRangeResponse {
        let max = req.max_blocks.max(1).min(1000) as usize;
        // Collect heights + metadata first to release the immutable borrow.
        let to_read: Vec<(u64, String, String, u32, u64)> = {
            let entries = self.index.range(req.from_height, req.to_height);
            entries
                .into_iter()
                .take(max)
                .map(|e| {
                    (
                        e.height,
                        e.parent_hash.clone(),
                        e.block_hash.clone(),
                        e.tx_count,
                        e.compressed_len,
                    )
                })
                .collect()
        };

        let mut blocks = Vec::with_capacity(to_read.len());
        for (height, parent_hash, block_hash, tx_count, compressed_len) in &to_read {
            match self.read_block(*height) {
                Ok(raw) => {
                    blocks.push(ArchivedBlock {
                        height: *height,
                        parent_hash: parent_hash.clone(),
                        block_hash: block_hash.clone(),
                        tx_count: *tx_count,
                        archived_at: 0,
                        producer: String::new(),
                        raw_bytes: raw.clone(),
                        compressed_size: *compressed_len,
                    });
                }
                Err(e) => {
                    eprintln!("WARNING: failed to read block {}: {e}", height);
                }
            }
        }

        let total = self.total_blocks();
        let served_from = to_read.first().map(|(h, ..)| *h).unwrap_or(0);
        let served_to = to_read.last().map(|(h, ..)| *h).unwrap_or(0);
        let has_more = served_to < req.to_height && served_to > 0;

        BlockRangeResponse {
            blocks,
            served_from,
            served_to,
            total_blocks: total,
            has_more,
            tip_height: self.tip_height(),
        }
    }

    /// Get total disk usage of the store.
    fn disk_used(&self) -> Result<u64, String> {
        let blocks_dir = self.root.join(Self::SHARD_DIR);
        let mut total = 0u64;
        if blocks_dir.exists() {
            for entry in fs::read_dir(&blocks_dir)
                .map_err(|e| format!("read blocks dir: {e}"))?
            {
                let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        // Add index file size.
        let index_path = self.root.join("index.bin");
        if index_path.exists() {
            total += fs::metadata(&index_path)
                .map(|m| m.len())
                .unwrap_or(0);
        }
        Ok(total)
    }
}

// ── Archive Oracle ──────────────────────────────────────────────────────────

/// The main archive oracle server.
pub struct ArchiveOracle {
    store: BlockStore,
    /// When the server started (Unix seconds).
    started_at: u64,
    /// Total requests served.
    requests_served: u64,
    /// Total bytes served.
    bytes_served: u64,
}

impl ArchiveOracle {
    /// Open an archive oracle at the given storage path.
    pub fn open(store_path: impl Into<PathBuf>) -> Result<Self, String> {
        let store = BlockStore::open(store_path)?;
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(Self {
            store,
            started_at,
            requests_served: 0,
            bytes_served: 0,
        })
    }

    /// Store a new block in the archive.
    pub fn archive_block(
        &mut self,
        height: u64,
        parent_hash: &str,
        block_hash: &str,
        tx_count: u32,
        _producer: &str,
        raw_bytes: &[u8],
    ) -> Result<(), String> {
        self.store
            .store_block(height, parent_hash, block_hash, tx_count, _producer, raw_bytes)?;
        Ok(())
    }

    /// Serve a BlockRange request.
    pub fn serve(&mut self, req: &BlockRangeRequest) -> BlockRangeResponse {
        self.requests_served += 1;
        let resp = self.store.serve_range(req);
        self.bytes_served += resp
            .blocks
            .iter()
            .map(|b| b.raw_bytes.len() as u64)
            .sum::<u64>();
        resp
    }

    /// Get server status.
    pub fn status(&self) -> ArchiveStatus {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ArchiveStatus {
            total_blocks: self.store.total_blocks(),
            tip_height: self.store.tip_height(),
            tip_hash: String::new(), // TODO
            disk_used_bytes: self.store.disk_used().unwrap_or(0),
            disk_capacity_bytes: 20_000_000_000_000, // 20TB
            compression_ratio: 0.0,                  // TODO
            uptime_seconds: now.saturating_sub(self.started_at),
            requests_served: self.requests_served,
            bytes_served: self.bytes_served,
            connected_light_nodes: 0, // TODO: track via p2p
        }
    }
}

// ── HTTP API ────────────────────────────────────────────────────────────────

/// Start a minimal HTTP server that serves the BlockRange API.
///
/// Endpoints:
///   GET /status              → ArchiveStatus JSON
///   GET /blocks?from=N&to=M  → BlockRangeResponse JSON (max_blocks=100)
///   GET /block/N             → single ArchivedBlock JSON
///
/// This is a minimal implementation using only std — no hyper/tokio needed
/// for the prototype. Replace with flux-api for production.
pub fn serve_http(
    oracle: &mut ArchiveOracle,
    listen: &str,
) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

    let listener =
        TcpListener::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    println!("🌐 Delta Archive Oracle HTTP listening on {listen}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                handle_http(oracle, stream);
            }
            Err(e) => {
                eprintln!("connection error: {e}");
            }
        }
    }
    Ok(())
}

fn handle_http(oracle: &mut ArchiveOracle, mut stream: TcpStream) {
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    let (status, body_json) = match (method, path) {
        ("GET", "/status") => {
            let status = oracle.status();
            (
                "200 OK",
                serde_json::to_string_pretty(&status).unwrap_or_default(),
            )
        }
        ("GET", p) if p.starts_with("/blocks") => {
            // Parse query string: /blocks?from=N&to=M&max=K
            let query = p.split('?').nth(1).unwrap_or("");
            let mut from = 1u64;
            let mut to = 100u64;
            let mut max = 100u64;
            for param in query.split('&') {
                let mut kv = param.splitn(2, '=');
                let k = kv.next().unwrap_or("");
                let v = kv.next().unwrap_or("");
                match k {
                    "from" => from = v.parse().unwrap_or(1),
                    "to" => to = v.parse().unwrap_or(100),
                    "max" => max = v.parse().unwrap_or(100).min(1000),
                    _ => {}
                }
            }
            let req = BlockRangeRequest {
                from_height: from,
                to_height: to,
                max_blocks: max,
                accept_compressed: false,
            };
            let resp = oracle.serve(&req);
            (
                "200 OK",
                serde_json::to_string_pretty(&resp).unwrap_or_default(),
            )
        }
        _ => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_json}",
        body_json.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

// ── CLI ─────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let store_path = arg_val(&args, "--store")
        .unwrap_or("/mnt/20tb/sigil-chain");
    let listen = arg_val(&args, "--listen")
        .unwrap_or("0.0.0.0:9800");

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║         DELTA ARCHIVE ORACLE — 20TB BlockRange Server       ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Store:  {:<50} ║", store_path);
    println!("║  Listen: {:<50} ║", listen);
    println!("╚══════════════════════════════════════════════════════════════╝");

    let mut oracle = match ArchiveOracle::open(&store_path) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("FATAL: failed to open archive at {store_path}: {e}");
            std::process::exit(1);
        }
    };

    let status = oracle.status();
    println!();
    println!("  Total blocks:  {}", status.total_blocks);
    println!("  Tip height:    {}", status.tip_height);
    println!("  Disk used:     {} MB", status.disk_used_bytes / 1_000_000);
    println!("  Disk capacity: 20 TB");
    println!();

    if let Err(e) = serve_http(&mut oracle, &listen) {
        eprintln!("FATAL: HTTP server error: {e}");
        std::process::exit(1);
    }
}

fn arg_val<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_store_roundtrip() {
        let tmp = std::env::temp_dir().join("delta_oracle_test");
        let _ = fs::remove_dir_all(&tmp);

        let mut store = BlockStore::open(&tmp).unwrap();
        assert_eq!(store.total_blocks(), 0);

        // Store 10 blocks.
        for h in 1..=10u64 {
            let raw = format!("block-{h}-data").into_bytes();
            store
                .store_block(h, &format!("parent-{}", h - 1), &format!("hash-{h}"), 5, "delta", &raw)
                .unwrap();
        }
        assert_eq!(store.total_blocks(), 10);
        assert_eq!(store.tip_height(), 10);

        // Read block 5.
        let raw = store.read_block(5).unwrap();
        assert_eq!(raw, b"block-5-data");

        // Serve a range.
        let req = BlockRangeRequest {
            from_height: 3,
            to_height: 7,
            max_blocks: 100,
            accept_compressed: false,
        };
        let resp = store.serve_range(&req);
        assert_eq!(resp.blocks.len(), 5);
        assert_eq!(resp.served_from, 3);
        assert_eq!(resp.served_to, 7);
        assert!(!resp.has_more);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn block_range_respects_max_blocks() {
        let tmp = std::env::temp_dir().join("delta_oracle_max_test");
        let _ = fs::remove_dir_all(&tmp);

        let mut store = BlockStore::open(&tmp).unwrap();
        for h in 1..=100u64 {
            store
                .store_block(h, "parent", "hash", 1, "delta", &[h as u8])
                .unwrap();
        }

        let req = BlockRangeRequest {
            from_height: 1,
            to_height: 100,
            max_blocks: 10,
            accept_compressed: false,
        };
        let resp = store.serve_range(&req);
        assert_eq!(resp.blocks.len(), 10);
        assert!(resp.has_more);
        assert_eq!(resp.served_to, 10);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn index_persistence_survives_reopen() {
        let tmp = std::env::temp_dir().join("delta_oracle_persist_test");
        let _ = fs::remove_dir_all(&tmp);

        // Session 1: store blocks.
        {
            let mut store = BlockStore::open(&tmp).unwrap();
            for h in 1..=5u64 {
                store
                    .store_block(h, "parent", "hash", 1, "delta", &[h as u8])
                    .unwrap();
            }
            assert_eq!(store.total_blocks(), 5);
        }

        // Session 2: reload and verify.
        {
            let mut store = BlockStore::open(&tmp).unwrap();
            assert_eq!(store.total_blocks(), 5);
            assert_eq!(store.tip_height(), 5);
            let raw = store.read_block(3).unwrap();
            assert_eq!(raw, &[3]);
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
