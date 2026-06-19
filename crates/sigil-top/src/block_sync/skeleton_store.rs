//! SkeletonStore — the flat, append-only store for the verified snapshot **skeleton prefix**.
//!
//! WHY: the commit bench proved flux-db KV caps bulk import at ~3.9k blk/s (per-key LSM
//! overhead), while a flat sequential append of 72-B records hits ~10M blk/s durable
//! (108× the 92.6k target). The verified prefix `[base, anchor]` is fold/anchor-authenticated
//! (B's read-trust contract), so it needs NO per-key store and NO per-record re-verify on read:
//! height → offset is implicit at a fixed 72-B stride. flux-db KV stays only for the live frontier.
//!
//! Record (sigil_header::SkeletonRecord, fixed 72 B under bincode):
//!   height:u64 ‖ block_hash:[u8;32] ‖ parent_hash:[u8;32]
//!   block_hash = BLAKE3(full header) (fold witness); parent_hash = spine link.
//!
//! DURABILITY (B's #493 note): 2-phase append — write the 72-B records, fsync, and only then
//! is the file length the authoritative count. A kill-9 mid-append can leave a torn tail; on
//! open we truncate to `floor(len/72)*72` so a partial record is never read. Heights are dense
//! (no gaps) — snapshot pages are contiguous; we ASSERT it on append (fail-loud).

use sigil_header::SkeletonRecord;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// On-disk record width. bincode of (u64, [u8;32], [u8;32]) = 8 + 32 + 32.
const REC: u64 = 72;

pub struct SkeletonStore {
    #[allow(dead_code)]
    path: PathBuf,
    base: u64,
    file: File,
    count: u64, // committed records = file_len / REC (after fsync)
}

impl SkeletonStore {
    /// Open (or create) the flat store anchored at `base`. Recovers from a torn tail by
    /// truncating to a whole number of records.
    pub fn open(path: impl AsRef<Path>, base: u64) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        let mut file = OpenOptions::new().read(true).write(true).create(true).open(&path)
            .map_err(|e| format!("skeleton open {}: {e}", path.display()))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let count = len / REC;
        if len % REC != 0 {
            // torn tail — keep only whole records
            file.set_len(count * REC).map_err(|e| format!("skeleton truncate: {e}"))?;
            let _ = file.sync_all();
        }
        Ok(Self { path, base, file, count })
    }

    /// Append a contiguous run of records (must start at `base + count`). 2-phase durable.
    /// Returns the number appended.
    pub fn append(&mut self, recs: &[SkeletonRecord]) -> Result<usize, String> {
        if recs.is_empty() { return Ok(0); }
        let expect_first = self.base + self.count;
        if recs[0].height != expect_first {
            return Err(format!("non-contiguous append: got h={} expected {}", recs[0].height, expect_first));
        }
        // serialize the whole run into one buffer (one write syscall), asserting fixed stride
        let mut buf = Vec::with_capacity(recs.len() * REC as usize);
        for (i, r) in recs.iter().enumerate() {
            if r.height != expect_first + i as u64 {
                return Err(format!("gap in run at index {i}: h={} expected {}", r.height, expect_first + i as u64));
            }
            let bytes = bincode::serialize(r).map_err(|e| format!("skeleton encode: {e}"))?;
            if bytes.len() as u64 != REC {
                return Err(format!("record not {REC}B (got {}) — layout drift, refusing", bytes.len()));
            }
            buf.extend_from_slice(&bytes);
        }
        self.file.seek(SeekFrom::Start(self.count * REC)).map_err(|e| e.to_string())?;
        self.file.write_all(&buf).map_err(|e| format!("skeleton write: {e}"))?;
        self.file.sync_all().map_err(|e| format!("skeleton fsync: {e}"))?; // phase 1: data durable
        self.count += recs.len() as u64; // phase 2: advance the authoritative count
        Ok(recs.len())
    }

    /// O(1) read by height — seek to the implicit offset, no index, no re-verify (prefix is
    /// fold-authenticated). None if out of range.
    pub fn read_at(&mut self, height: u64) -> Result<Option<SkeletonRecord>, String> {
        if height < self.base || height >= self.base + self.count {
            return Ok(None);
        }
        let off = (height - self.base) * REC;
        self.file.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
        let mut b = [0u8; REC as usize];
        self.file.read_exact(&mut b).map_err(|e| format!("skeleton read: {e}"))?;
        let rec = bincode::deserialize(&b).map_err(|e| format!("skeleton decode: {e}"))?;
        Ok(Some(rec))
    }

    /// Highest stored height, or None if empty.
    pub fn tip_height(&self) -> Option<u64> {
        if self.count == 0 { None } else { Some(self.base + self.count - 1) }
    }

    pub fn count(&self) -> u64 { self.count }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(h: u64) -> SkeletonRecord {
        SkeletonRecord { height: h, block_hash: [h as u8; 32], parent_hash: [(h.wrapping_sub(1)) as u8; 32] }
    }

    #[test]
    fn append_read_roundtrip_and_offsets() {
        let p = std::env::temp_dir().join(format!("skel-rt-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s = SkeletonStore::open(&p, 1).unwrap();
        let run: Vec<_> = (1..=1000).map(rec).collect();
        assert_eq!(s.append(&run).unwrap(), 1000);
        assert_eq!(s.tip_height(), Some(1000));
        // O(1) random reads land on the right record
        assert_eq!(s.read_at(1).unwrap().unwrap().height, 1);
        assert_eq!(s.read_at(500).unwrap().unwrap().height, 500);
        assert_eq!(s.read_at(1000).unwrap().unwrap().block_hash, rec(1000).block_hash);
        assert!(s.read_at(1001).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn non_contiguous_append_is_rejected() {
        let p = std::env::temp_dir().join(format!("skel-nc-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s = SkeletonStore::open(&p, 1).unwrap();
        assert!(s.append(&[rec(2)]).is_err(), "starting above base+count must fail");
        s.append(&[rec(1)]).unwrap();
        assert!(s.append(&[rec(3)]).is_err(), "gap must fail (expected 2)");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_tail_is_truncated_on_open() {
        let p = std::env::temp_dir().join(format!("skel-torn-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        { let mut s = SkeletonStore::open(&p, 1).unwrap(); s.append(&(1..=10).map(rec).collect::<Vec<_>>()).unwrap(); }
        // simulate a kill-9 mid-append: append 40 stray bytes (a torn 11th record)
        { let mut f = OpenOptions::new().append(true).open(&p).unwrap(); f.write_all(&[0u8; 40]).unwrap(); }
        let s = SkeletonStore::open(&p, 1).unwrap();
        assert_eq!(s.count(), 10, "torn tail dropped — only whole records survive");
        let _ = std::fs::remove_file(&p);
    }
}
