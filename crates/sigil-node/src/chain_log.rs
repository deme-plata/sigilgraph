//! chain_log.rs — append-only on-disk block log (the memory-bound persistence).
//!
//! The old persistence held the WHOLE chain in RAM (`ChainTip.blocks: Vec<Block>`)
//! and snapshotted/replayed all of it through aether → the producer OOM-killed in a
//! crash loop as the chain grew (588 MB store → out of memory on recovery).
//!
//! This log persists every block as `[u32 little-endian length][serde_json bytes]`
//! appended to `chain.log`, with an in-RAM `offsets` index (one u64 per block, tiny
//! vs the blocks themselves). It enables:
//!   * O(1) append on each applied block (no full-chain re-serialize),
//!   * O(1) `get(height)` for serving backfill of OLD blocks straight from disk,
//!   * streaming `replay()` on recovery — one block at a time, BOUNDED RAM.
//! The in-RAM chain ([`crate::chain::ChainTip`]) then keeps only a small recent
//! WINDOW; everything older lives here on disk.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::block::Block;

pub struct ChainLog {
    path: PathBuf,
    writer: BufWriter<File>,
    /// `offsets[h]` = byte offset of block `h`'s record in the log.
    offsets: Vec<u64>,
    bytes_len: u64,
}

impl ChainLog {
    /// Open (creating if absent) and build the offset index by scanning records.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("chain.log");
        let mut offsets = Vec::new();
        let mut bytes_len = 0u64;
        if path.exists() {
            let mut r = BufReader::new(File::open(&path)?);
            let mut pos = 0u64;
            loop {
                let mut lb = [0u8; 4];
                if r.read_exact(&mut lb).is_err() {
                    break; // clean EOF (or a torn trailing record — stop there)
                }
                let rec = u32::from_le_bytes(lb) as u64;
                offsets.push(pos);
                pos += 4 + rec;
                if r.seek(SeekFrom::Start(pos)).is_err() {
                    offsets.pop(); // torn record at the tail; drop it
                    break;
                }
            }
            bytes_len = pos;
        }
        let writer = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path)?);
        Ok(Self { path, writer, offsets, bytes_len })
    }

    /// Number of blocks on disk (== next expected height).
    pub fn height(&self) -> u64 {
        self.offsets.len() as u64
    }

    /// Append one block. O(1) — no full-chain rewrite.
    pub fn append(&mut self, block: &Block) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(block)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.append_bytes(&bytes)
    }

    /// Append a block already serialized to its serde_json bytes (the live path
    /// reuses the broadcast/gossip bytes — no re-serialize, no clone).
    pub fn append_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let lb = (bytes.len() as u32).to_le_bytes();
        self.writer.write_all(&lb)?;
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        self.offsets.push(self.bytes_len);
        self.bytes_len += 4 + bytes.len() as u64;
        Ok(())
    }

    /// Read block at `height` from disk (for serving backfill of pruned blocks).
    pub fn get(&self, height: u64) -> Option<Block> {
        let off = *self.offsets.get(height as usize)?;
        let mut f = File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut lb = [0u8; 4];
        f.read_exact(&mut lb).ok()?;
        let n = u32::from_le_bytes(lb) as usize;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf).ok()?;
        serde_json::from_slice(&buf).ok()
    }

    /// Read a contiguous height range `[from..=to]` from disk with ONE file open +
    /// sequential read (vs `get()` which opens the file per height — 8192 opens/chunk
    /// was a serve bottleneck). Stops at the end of the log.
    pub fn get_range(&self, from: u64, to: u64) -> Vec<Block> {
        let start = match self.offsets.get(from as usize) { Some(o) => *o, None => return Vec::new() };
        let mut out = Vec::new();
        let f = match File::open(&self.path) { Ok(f) => f, Err(_) => return out };
        let mut r = BufReader::new(f);
        if r.seek(SeekFrom::Start(start)).is_err() { return out; }
        let last = (to as usize).min(self.offsets.len().saturating_sub(1));
        for _ in from as usize..=last {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() { break; }
            let n = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() { break; }
            match serde_json::from_slice::<Block>(&buf) {
                Ok(b) => out.push(b),
                Err(_) => break,
            }
        }
        out
    }

    /// Stream every block in order, invoking `f` per block. Bounded RAM (one block
    /// in flight) — used to rebuild state on recovery without loading the chain.
    pub fn replay<F: FnMut(Block)>(dir: &Path, mut f: F) -> std::io::Result<u64> {
        let path = dir.join("chain.log");
        if !path.exists() {
            return Ok(0);
        }
        let mut r = BufReader::new(File::open(&path)?);
        let mut n = 0u64;
        loop {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() {
                break;
            }
            let len = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; len];
            if r.read_exact(&mut buf).is_err() {
                break; // torn tail record
            }
            match serde_json::from_slice::<Block>(&buf) {
                Ok(b) => {
                    f(b);
                    n += 1;
                }
                Err(_) => break,
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_get_replay_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-chainlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks = crate::block::__test_chain(50);
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for b in &blocks {
                log.append(b).unwrap();
            }
            assert_eq!(log.height(), 50);
            // random access from disk
            let g = log.get(7).unwrap();
            assert_eq!(g.header.height, blocks[7].header.height);
        }
        // reopen rebuilds the offset index from disk
        let log2 = ChainLog::open(&dir).unwrap();
        assert_eq!(log2.height(), 50);
        assert_eq!(log2.get(49).unwrap().header.height, blocks[49].header.height);
        // streaming replay sees all, in order, bounded RAM
        let mut seen = 0u64;
        let n = ChainLog::replay(&dir, |_b| { seen += 1; });
        assert_eq!(n.unwrap(), 50);
        assert_eq!(seen, 50);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
