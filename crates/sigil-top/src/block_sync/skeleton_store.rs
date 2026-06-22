//! SkeletonStore — SIGIL's flat append-only skeleton-prefix store.
//!
//! As of 2026-06-22 the durability engine lives natively in **flux-db**
//! (`flux_db::skeleton::SkeletonStore`) — the flat, fixed-stride, 2-phase-durable,
//! torn-tail-recovering append store that bulk-imports a verified prefix at ~10M blk/s
//! (vs flux-db KV's ~3.9k blk/s per-key wall). This file is now a thin SIGIL-flavoured
//! adapter over that primitive: it teaches `sigil_header::SkeletonRecord` the flux-db
//! `SkeletonRecord` layout and keeps SIGIL's "height" vocabulary on the wrapper.
//!
//! Record (`sigil_header::SkeletonRecord`, fixed 72 B):
//!   height:u64(LE) ‖ block_hash:[u8;32] ‖ parent_hash:[u8;32]
//!   block_hash = BLAKE3(full header) (fold witness); parent_hash = spine link.
//!   (LE-fixint layout is byte-identical to the prior bincode default, so existing
//!    skeleton files and `archive_root` witnesses are unchanged.)
//!
//! What flux-db now owns (so SIGIL doesn't re-implement it): contiguity asserts, the
//! 2-phase fsync, torn-tail truncation on open, O(1) `read_at`, and the new
//! fast-sync extras surfaced below — bulk `read_range` (snapshot-page serving),
//! streaming BLAKE3 `archive_root` (the fold/anchor witness), and the unsynced
//! max-throughput import path.

use sigil_header::SkeletonRecord;
use std::path::Path;

/// On-disk record width — must match `SkelRec::WIDTH` and the SIGIL wire layout.
const REC: usize = 72;

/// Local newtype so we can implement flux-db's `SkeletonRecord` trait for SIGIL's record
/// without tripping the orphan rule (both the trait and `sigil_header::SkeletonRecord` are
/// foreign to this crate). `repr(transparent)` lets us reinterpret a `&[SkeletonRecord]`
/// as `&[SkelRec]` with zero copy on the bulk-append hot path.
#[repr(transparent)]
#[derive(Clone)]
struct SkelRec(SkeletonRecord);

impl flux_db::SkeletonRecord for SkelRec {
    const WIDTH: usize = REC;

    #[inline]
    fn seq(&self) -> u64 {
        self.0.height
    }

    #[inline]
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.0.height.to_le_bytes());
        out[8..40].copy_from_slice(&self.0.block_hash);
        out[40..72].copy_from_slice(&self.0.parent_hash);
    }

    fn decode(buf: &[u8]) -> Result<Self, String> {
        if buf.len() != REC {
            return Err(format!("SkeletonRecord: need {REC} bytes, got {}", buf.len()));
        }
        let mut h = [0u8; 8];
        h.copy_from_slice(&buf[0..8]);
        let mut block_hash = [0u8; 32];
        block_hash.copy_from_slice(&buf[8..40]);
        let mut parent_hash = [0u8; 32];
        parent_hash.copy_from_slice(&buf[40..72]);
        Ok(SkelRec(SkeletonRecord {
            height: u64::from_le_bytes(h),
            block_hash,
            parent_hash,
        }))
    }
}

/// Reinterpret `&[SkeletonRecord]` as `&[SkelRec]` without copying.
///
/// Safety: `SkelRec` is `#[repr(transparent)]` over `SkeletonRecord`, so the two have
/// identical size and layout; a slice of one is a valid slice of the other.
#[inline]
fn as_skel(recs: &[SkeletonRecord]) -> &[SkelRec] {
    unsafe { std::slice::from_raw_parts(recs.as_ptr() as *const SkelRec, recs.len()) }
}

/// SIGIL skeleton store — delegates to the flux-db native primitive.
pub struct SkeletonStore {
    inner: flux_db::SkeletonStore<SkelRec>,
}

impl SkeletonStore {
    /// Open (or create) the flat store anchored at `base` (= the snapshot anchor height).
    pub fn open(path: impl AsRef<Path>, base: u64) -> Result<Self, String> {
        Ok(Self {
            inner: flux_db::SkeletonStore::open(path, base)?,
        })
    }

    /// Append a contiguous run (must start at `base + count`), 2-phase durable.
    pub fn append(&mut self, recs: &[SkeletonRecord]) -> Result<usize, String> {
        self.inner.append(as_skel(recs))
    }

    /// Append a contiguous run WITHOUT fsync — the max-throughput bulk-import path.
    /// Pair with [`sync`](Self::sync) after a batch of pages.
    pub fn append_unsynced(&mut self, recs: &[SkeletonRecord]) -> Result<usize, String> {
        self.inner.append_unsynced(as_skel(recs))
    }

    /// Flush unsynced appends to stable storage.
    pub fn sync(&mut self) -> Result<(), String> {
        self.inner.sync()
    }

    /// O(1) read by height. `None` if out of range.
    pub fn read_at(&mut self, height: u64) -> Result<Option<SkeletonRecord>, String> {
        Ok(self.inner.read_at(height)?.map(|r| r.0))
    }

    /// Bulk read the inclusive height range `[from, to]` in one seek — the snapshot-page
    /// serving path for the sync responder. Clamps to what's stored.
    pub fn read_range(&mut self, from: u64, to: u64) -> Result<Vec<SkeletonRecord>, String> {
        Ok(self
            .inner
            .read_range(from, to)?
            .into_iter()
            .map(|r| r.0)
            .collect())
    }

    /// Streaming BLAKE3 commitment over the whole stored prefix — the fold/anchor witness
    /// a light client checks against a signed anchor before trusting any `read_at`.
    pub fn archive_root(&mut self) -> Result<[u8; 32], String> {
        self.inner.archive_root()
    }

    /// Highest stored height, or `None` if empty.
    pub fn tip_height(&self) -> Option<u64> {
        self.inner.tip_seq()
    }

    pub fn count(&self) -> u64 {
        self.inner.count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn rec(h: u64) -> SkeletonRecord {
        SkeletonRecord {
            height: h,
            block_hash: [h as u8; 32],
            parent_hash: [(h.wrapping_sub(1)) as u8; 32],
        }
    }

    #[test]
    fn append_read_roundtrip_and_offsets() {
        let p = std::env::temp_dir().join(format!("skel-rt-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s = SkeletonStore::open(&p, 1).unwrap();
        let run: Vec<_> = (1..=1000).map(rec).collect();
        assert_eq!(s.append(&run).unwrap(), 1000);
        assert_eq!(s.tip_height(), Some(1000));
        assert_eq!(s.read_at(1).unwrap().unwrap().height, 1);
        assert_eq!(s.read_at(500).unwrap().unwrap(), rec(500));
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
        {
            let mut s = SkeletonStore::open(&p, 1).unwrap();
            s.append(&(1..=10).map(rec).collect::<Vec<_>>()).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&[0u8; 40]).unwrap();
        }
        let s = SkeletonStore::open(&p, 1).unwrap();
        assert_eq!(s.count(), 10, "torn tail dropped — only whole records survive");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_range_serves_pages_and_root_is_stable() {
        let p = std::env::temp_dir().join(format!("skel-rr-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s = SkeletonStore::open(&p, 0).unwrap();
        s.append(&(0..=500).map(rec).collect::<Vec<_>>()).unwrap();
        let page = s.read_range(100, 199).unwrap();
        assert_eq!(page.len(), 100);
        assert_eq!(page[0], rec(100));
        assert_eq!(page[99], rec(199));
        let root_a = s.archive_root().unwrap();
        // re-open and re-hash → identical witness
        drop(s);
        let mut s2 = SkeletonStore::open(&p, 0).unwrap();
        assert_eq!(s2.archive_root().unwrap(), root_a, "fold witness stable across reopen");
        let _ = std::fs::remove_file(&p);
    }
}
