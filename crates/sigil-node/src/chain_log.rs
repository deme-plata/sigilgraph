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

/// `chain.idx` — sparse on-disk height→offset index alongside `chain.log`.
///
/// Layout: 8-byte header (`b"SGLIDX\0"` + 1 version byte) followed by
/// fixed-size little-endian entries `[height: u64][offset: u64]` (16 B each),
/// one entry every [`IDX_EVERY`] appended blocks. At 21M blocks that's ~41K
/// entries (~670 KB) — tiny, and enough to start a tail-replay within one
/// 512-block stride of the target instead of scanning 52 GB from byte 0.
///
/// The index is strictly best-effort: [`ChainLog::replay_from`] validates the
/// entry it picks (re-reads the block at that offset and checks the height
/// matches) and falls back to a full filtered scan — rebuilding the index as a
/// side effect — if the file is missing, torn, stale, or lying.
const IDX_MAGIC: [u8; 7] = *b"SGLIDX\0";
const IDX_VERSION: u8 = 1;
const IDX_HEADER_LEN: usize = 8;
const IDX_ENTRY_LEN: usize = 16;
/// One index entry per this many appended blocks (sparse — keeps appends hot:
/// 511 of every 512 appends don't touch the index at all; the 512th pays one
/// tiny probe-parse + a flushed 16-byte write).
const IDX_EVERY: u64 = 512;

/// Minimal deserialization target to pull `header.height` out of a block's
/// serde_json bytes without decoding the whole block (used on the append path
/// only once every [`IDX_EVERY`] blocks, and when validating an index entry).
#[derive(serde::Deserialize)]
struct HeightProbe {
    header: HeaderHeightProbe,
}
#[derive(serde::Deserialize)]
struct HeaderHeightProbe {
    height: u64,
}

/// Fast height extraction: find the first `"height":<digits>` in the record's
/// leading bytes. `header` is the block's first field and `height` its third,
/// so the first occurrence IS `header.height` (the only other height-ish key,
/// `"at_height"`, has no quote before the `h` and can't match). Used to skip
/// pre-`from_height` records during tail-replay catch-up without paying a full
/// serde_json decode per skipped block (~0.5 ms each → seconds per stride).
/// Returns `None` on any doubt — callers then fall back to a real parse, so a
/// wrong/missing probe can never change which blocks are applied.
fn probe_height_fast(bytes: &[u8]) -> Option<u64> {
    const KEY: &[u8] = b"\"height\":";
    let window = &bytes[..bytes.len().min(PROBE_WINDOW)];
    let at = window.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let mut val: u64 = 0;
    let mut any = false;
    let mut terminated = false;
    for &c in &window[at..] {
        if c.is_ascii_digit() {
            val = val.checked_mul(10)?.checked_add((c - b'0') as u64)?;
            any = true;
        } else {
            terminated = true;
            break;
        }
    }
    // Digits must END inside the window — a digit run cut off by the window
    // edge (probe fed only a record prefix) would yield a truncated, too-small
    // height and could skip a block we must apply. Refuse instead.
    if any && terminated { Some(val) } else { None }
}

/// How many leading bytes of a record the skip-probe reads/searches.
/// `header.height` sits ~40-80 bytes in (header is the block's first field;
/// only `version` and the 8-byte `network_id` array precede it), so 256 bytes
/// is generous headroom while keeping the per-record probe cost trivial — the
/// catch-up scan `seek_relative`s over the rest of each skipped record. If the
/// key ever moves past the window the probe returns `None` and the scan falls
/// back to a full decode for that record (slower, never wrong).
const PROBE_WINDOW: usize = 256;

pub struct ChainLog {
    path: PathBuf,
    writer: BufWriter<File>,
    /// `offsets[h]` = byte offset of block `h`'s record in the log.
    offsets: Vec<u64>,
    bytes_len: u64,
    /// Best-effort append handle for `chain.idx`. `None` = index writes are
    /// disabled (open/IO error) — reads then fall back to filtered scans.
    idx_writer: Option<BufWriter<File>>,
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
        let idx_writer = Self::open_idx_writer(&Self::idx_path_for(&path), offsets.is_empty());
        Ok(Self { path, writer, offsets, bytes_len, idx_writer })
    }

    /// `chain.idx` lives next to `chain.log`.
    fn idx_path_for(log_path: &Path) -> PathBuf {
        log_path.with_file_name("chain.idx")
    }

    /// Open (or create) the index for appending. Best-effort — any failure
    /// returns `None` and the log keeps working without index writes.
    /// If the log is empty (fresh dir) any stale index is discarded.
    fn open_idx_writer(idx_path: &Path, log_is_empty: bool) -> Option<BufWriter<File>> {
        if log_is_empty {
            let _ = std::fs::remove_file(idx_path); // stale index for a gone log
        }
        let valid_existing = !log_is_empty
            && std::fs::File::open(idx_path)
                .ok()
                .map(|mut f| {
                    let mut hdr = [0u8; IDX_HEADER_LEN];
                    f.read_exact(&mut hdr).is_ok()
                        && hdr[..7] == IDX_MAGIC
                        && hdr[7] == IDX_VERSION
                })
                .unwrap_or(false);
        let file = if valid_existing {
            OpenOptions::new().append(true).open(idx_path).ok()?
        } else {
            // Missing or unrecognized — start fresh with just the header.
            // (Entries for already-logged blocks are absent; replay_from
            // self-heals by rebuilding on its fallback path.)
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(idx_path)
                .ok()?;
            let mut hdr = [0u8; IDX_HEADER_LEN];
            hdr[..7].copy_from_slice(&IDX_MAGIC);
            hdr[7] = IDX_VERSION;
            f.write_all(&hdr).ok()?;
            f
        };
        Some(BufWriter::new(file))
    }

    /// Append one `(height, offset)` entry to `chain.idx`. Best-effort: on any
    /// IO error the index writer is dropped and appends continue un-indexed.
    fn write_idx_entry(&mut self, height: u64, offset: u64) {
        if let Some(w) = self.idx_writer.as_mut() {
            let mut e = [0u8; IDX_ENTRY_LEN];
            e[..8].copy_from_slice(&height.to_le_bytes());
            e[8..].copy_from_slice(&offset.to_le_bytes());
            if w.write_all(&e).and_then(|_| w.flush()).is_err() {
                self.idx_writer = None;
            }
        }
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
        // Sparse height→offset index entry, one per IDX_EVERY blocks. The probe
        // parse + 16-byte write happen on 1/4096 of appends — the hot path is
        // untouched for the other 4095.
        if (self.offsets.len() as u64) % IDX_EVERY == 0 {
            if let Ok(p) = serde_json::from_slice::<HeightProbe>(bytes) {
                let off = self.bytes_len;
                self.write_idx_entry(p.header.height, off);
            }
        }
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

    /// Replay only blocks with height >= from_height, in order, calling f per block.
    /// Returns the number of blocks applied. Falls back to a full scan with a
    /// height filter if the offset index is missing/corrupt (never errors on a
    /// missing index — self-heals by rebuilding or filtering).
    ///
    /// Fast path: read the sparse `chain.idx`, pick the entry with the largest
    /// `height <= from_height`, validate it (re-decode the block at that offset
    /// and confirm the height matches — a stale/lying index can't skip blocks),
    /// seek there, then scan forward filtering on `header.height >= from_height`.
    /// At IDX_EVERY=4096 the scan overshoot is at most one stride, so locating
    /// the start is O(stride) regardless of log size.
    ///
    /// Assumes block heights are non-decreasing in append order (true for the
    /// chain log — recovery refuses out-of-order logs upstream).
    pub fn replay_from(
        dir: &std::path::Path,
        from_height: u64,
        mut f: impl FnMut(crate::block::Block),
    ) -> Result<u64, String> {
        let log_path = dir.join("chain.log");
        if !log_path.exists() {
            return Ok(0);
        }
        if let Some(start) = Self::idx_seek_offset(dir, from_height, &log_path) {
            return Self::scan_filtered(&log_path, start, from_height, &mut f);
        }
        // Index missing/corrupt/stale → full scan with a height filter,
        // rebuilding chain.idx as we go (self-heal).
        Self::full_scan_rebuild(dir, &log_path, from_height, &mut f)
    }

    /// Resolve `from_height` to a safe byte offset to start scanning from,
    /// using `chain.idx`. Returns:
    ///   * `Some(offset)` — validated start (or 0 when no entry covers
    ///     `from_height` yet, e.g. `from_height == 0` or a gappy index — a
    ///     filtered scan from 0 is always correct, just slower),
    ///   * `None` — index unusable (missing / bad header / entry fails
    ///     validation against the log) → caller should fall back + rebuild.
    fn idx_seek_offset(dir: &Path, from_height: u64, log_path: &Path) -> Option<u64> {
        let raw = std::fs::read(Self::idx_path_for(&dir.join("chain.log"))).ok()?;
        if raw.len() < IDX_HEADER_LEN || raw[..7] != IDX_MAGIC || raw[7] != IDX_VERSION {
            return None;
        }
        // Best entry = largest height <= from_height. Iterate all (file is tiny,
        // ~16 B per 4096 blocks); chunks_exact silently drops a torn tail entry.
        let mut best: Option<(u64, u64)> = None; // (height, offset)
        for e in raw[IDX_HEADER_LEN..].chunks_exact(IDX_ENTRY_LEN) {
            let h = u64::from_le_bytes(e[..8].try_into().unwrap());
            let off = u64::from_le_bytes(e[8..].try_into().unwrap());
            if h <= from_height && best.map(|(bh, _)| h >= bh).unwrap_or(true) {
                best = Some((h, off));
            }
        }
        let (h, off) = match best {
            None => return Some(0), // nothing indexed below from_height — scan from start
            Some(b) => b,
        };
        // Validate: the record at `off` must decode and carry exactly height `h`.
        // Catches a stale index left over from a truncated/recreated log.
        let mut r = File::open(log_path).ok()?;
        r.seek(SeekFrom::Start(off)).ok()?;
        let mut lb = [0u8; 4];
        r.read_exact(&mut lb).ok()?;
        let n = u32::from_le_bytes(lb) as usize;
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).ok()?;
        let probe: HeightProbe = serde_json::from_slice(&buf).ok()?;
        if probe.header.height != h {
            return None;
        }
        Some(off)
    }

    /// Scan the log from `start` to EOF, calling `f` for every block whose
    /// height >= from_height. Same record framing + torn-tail tolerance as
    /// `replay()`.
    fn scan_filtered(
        log_path: &Path,
        start: u64,
        from_height: u64,
        f: &mut impl FnMut(Block),
    ) -> Result<u64, String> {
        let file = File::open(log_path).map_err(|e| format!("open chain.log: {}", e))?;
        let mut r = BufReader::new(file);
        r.seek(SeekFrom::Start(start)).map_err(|e| format!("seek chain.log: {}", e))?;
        let mut n = 0u64;
        let mut head = vec![0u8; PROBE_WINDOW]; // reused probe scratch
        loop {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() {
                break; // clean EOF
            }
            let len = u32::from_le_bytes(lb) as usize;
            // Catch-up fast path: read only the record's head, byte-probe the
            // height, and seek over the payload of records still below
            // from_height (full decode is ~0.5 ms/record — the probe+seek is
            // microseconds). If the probe is unsure (None) we fall through to
            // the real parse — the applied set is always decided by the decoded
            // header, never the probe.
            if from_height > 0 {
                let head_len = len.min(PROBE_WINDOW);
                if r.read_exact(&mut head[..head_len]).is_err() {
                    break; // torn tail record
                }
                match probe_height_fast(&head[..head_len]) {
                    Some(h) if h < from_height => {
                        if r.seek_relative((len - head_len) as i64).is_err() {
                            break;
                        }
                        continue;
                    }
                    _ => {
                        // Need this record in full: head + remainder.
                        let mut buf = vec![0u8; len];
                        buf[..head_len].copy_from_slice(&head[..head_len]);
                        if r.read_exact(&mut buf[head_len..]).is_err() {
                            break; // torn tail record
                        }
                        match serde_json::from_slice::<Block>(&buf) {
                            Ok(b) => {
                                if b.header.height >= from_height {
                                    f(b);
                                    n += 1;
                                }
                            }
                            Err(_) => break,
                        }
                        continue;
                    }
                }
            }
            // from_height == 0: byte-identical behavior to replay().
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

    /// Fallback: full scan from byte 0 with a height filter AND rebuild
    /// `chain.idx` from what we see (one entry per IDX_EVERY records, same
    /// cadence as the append path). The rebuilt index is written to a temp
    /// file then renamed — a crash mid-rebuild leaves no half-written index.
    /// Rebuild failures are swallowed: the replay result is already correct.
    fn full_scan_rebuild(
        dir: &Path,
        log_path: &Path,
        from_height: u64,
        f: &mut impl FnMut(Block),
    ) -> Result<u64, String> {
        let file = File::open(log_path).map_err(|e| format!("open chain.log: {}", e))?;
        let mut r = BufReader::new(file);
        let mut n = 0u64; // blocks passed to f
        let mut pos = 0u64; // record index (log position)
        let mut byte_off = 0u64;
        let mut entries: Vec<(u64, u64)> = Vec::new();
        let mut head = vec![0u8; PROBE_WINDOW]; // reused probe scratch
        loop {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() {
                break;
            }
            let len = u32::from_le_bytes(lb) as usize;
            let need_entry = pos % IDX_EVERY == 0;
            let head_len = len.min(PROBE_WINDOW);
            if r.read_exact(&mut head[..head_len]).is_err() {
                break; // torn tail record
            }
            // Same catch-up fast path as scan_filtered: probe the head, seek
            // over pre-from_height payloads (the probe height is also exactly
            // what the append path would have indexed for this record).
            let mut skipped = false;
            if from_height > 0 {
                if let Some(h) = probe_height_fast(&head[..head_len]) {
                    if h < from_height {
                        if need_entry {
                            entries.push((h, byte_off));
                        }
                        if r.seek_relative((len - head_len) as i64).is_err() {
                            break;
                        }
                        skipped = true;
                    }
                }
            }
            if !skipped {
                let mut buf = vec![0u8; len];
                buf[..head_len].copy_from_slice(&head[..head_len]);
                if r.read_exact(&mut buf[head_len..]).is_err() {
                    break; // torn tail record
                }
                match serde_json::from_slice::<Block>(&buf) {
                    Ok(b) => {
                        if need_entry {
                            entries.push((b.header.height, byte_off));
                        }
                        if b.header.height >= from_height {
                            f(b);
                            n += 1;
                        }
                    }
                    Err(_) => break,
                }
            }
            pos += 1;
            byte_off += 4 + len as u64;
        }
        // Self-heal: persist the rebuilt index (best-effort).
        let idx_path = Self::idx_path_for(&dir.join("chain.log"));
        let tmp = idx_path.with_extension("idx.tmp");
        let write_ok = (|| -> std::io::Result<()> {
            let mut w = BufWriter::new(File::create(&tmp)?);
            let mut hdr = [0u8; IDX_HEADER_LEN];
            hdr[..7].copy_from_slice(&IDX_MAGIC);
            hdr[7] = IDX_VERSION;
            w.write_all(&hdr)?;
            for (h, off) in &entries {
                let mut e = [0u8; IDX_ENTRY_LEN];
                e[..8].copy_from_slice(&h.to_le_bytes());
                e[8..].copy_from_slice(&off.to_le_bytes());
                w.write_all(&e)?;
            }
            w.flush()?;
            std::fs::rename(&tmp, &idx_path)
        })()
        .is_ok();
        if !write_ok {
            let _ = std::fs::remove_file(&tmp);
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

    /// Build a 10k-block log in a fresh temp dir (heights 1..=10_000 per
    /// `__test_chain`) and return the dir. Caller cleans up.
    fn build_10k_log(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sigil-chainlog-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks = crate::block::__test_chain(10_000);
        let mut log = ChainLog::open(&dir).unwrap();
        for b in &blocks {
            log.append(b).unwrap();
        }
        assert_eq!(log.height(), 10_000);
        dir
    }

    /// Expected chain.idx size for a 10k-block log: header + one entry per
    /// IDX_EVERY appends (positions 0, IDX_EVERY, 2*IDX_EVERY, … < 10_000).
    fn expected_idx_len_10k() -> u64 {
        let entries = (10_000 - 1) / IDX_EVERY + 1;
        (IDX_HEADER_LEN as u64) + entries * (IDX_ENTRY_LEN as u64)
    }

    /// eprintln a seek timing AND append it to a temp file — the test harness
    /// swallows output of PASSING tests, so the timing would otherwise only be
    /// visible when the <10ms assertion fails.
    fn log_seek_timing(label: &str, seek: std::time::Duration) {
        let line = format!("{}: located start in {:?}", label, seek);
        eprintln!("{}", line);
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("sigil-chainlog-seek-timing.log"))
        {
            let _ = writeln!(f, "{}", line);
        }
    }

    #[test]
    fn replay_from_tail_yields_last_100_in_order_and_seeks_fast() {
        let dir = build_10k_log("tail");
        let idx = dir.join("chain.idx");
        assert!(idx.exists());
        assert_eq!(std::fs::metadata(&idx).unwrap().len(), expected_idx_len_10k());

        // Heights run 1..=10_000, so the last 100 blocks are 9_901..=10_000.
        let t0 = std::time::Instant::now();
        let mut first_block_at: Option<std::time::Duration> = None;
        let mut heights = Vec::new();
        let n = ChainLog::replay_from(&dir, 9_901, |b| {
            if first_block_at.is_none() {
                first_block_at = Some(t0.elapsed());
            }
            heights.push(b.header.height);
        })
        .unwrap();
        assert_eq!(n, 100);
        assert_eq!(heights.len(), 100);
        assert_eq!(heights.first(), Some(&9_901));
        assert_eq!(heights.last(), Some(&10_000));
        assert!(heights.windows(2).all(|w| w[0] + 1 == w[1]), "blocks out of order");
        let seek = first_block_at.unwrap();
        log_seek_timing("replay_from(9_901) indexed seek", seek);
        assert!(seek < std::time::Duration::from_millis(10), "seek took {:?} (>10ms)", seek);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_from_zero_matches_full_replay() {
        let dir = build_10k_log("zero");
        let full = ChainLog::replay(&dir, |_b| {}).unwrap();
        let mut seen = 0u64;
        let from0 = ChainLog::replay_from(&dir, 0, |_b| { seen += 1; }).unwrap();
        assert_eq!(full, 10_000);
        assert_eq!(from0, full);
        assert_eq!(seen, full);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_from_missing_index_falls_back_and_self_heals() {
        let dir = build_10k_log("heal");
        let idx = dir.join("chain.idx");
        std::fs::remove_file(&idx).unwrap();
        assert!(!idx.exists());

        // Fallback: full filtered scan still yields exactly the tail…
        let mut heights = Vec::new();
        let n = ChainLog::replay_from(&dir, 9_901, |b| heights.push(b.header.height)).unwrap();
        assert_eq!(n, 100);
        assert_eq!(heights.first(), Some(&9_901));
        assert_eq!(heights.last(), Some(&10_000));

        // …and rebuilds the index as a side effect (self-heal):
        assert!(idx.exists(), "fallback should rebuild chain.idx");
        assert_eq!(std::fs::metadata(&idx).unwrap().len(), expected_idx_len_10k());

        // The healed index now serves a fast seek again.
        let t0 = std::time::Instant::now();
        let mut first_block_at: Option<std::time::Duration> = None;
        let n2 = ChainLog::replay_from(&dir, 9_901, |_b| {
            if first_block_at.is_none() {
                first_block_at = Some(t0.elapsed());
            }
        })
        .unwrap();
        assert_eq!(n2, 100);
        let seek = first_block_at.unwrap();
        log_seek_timing("replay_from(9_901) after self-heal", seek);
        assert!(seek < std::time::Duration::from_millis(10), "seek took {:?} (>10ms)", seek);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_from_corrupt_index_falls_back() {
        let dir = build_10k_log("corrupt");
        let idx = dir.join("chain.idx");
        // Garbage header → index unusable → filtered full scan, then rebuilt.
        std::fs::write(&idx, b"NOTANIDXFILE!!!!").unwrap();
        let mut n_seen = 0u64;
        let n = ChainLog::replay_from(&dir, 9_901, |_b| { n_seen += 1; }).unwrap();
        assert_eq!(n, 100);
        assert_eq!(n_seen, 100);
        // Rebuilt with a valid header.
        let raw = std::fs::read(&idx).unwrap();
        assert_eq!(&raw[..7], b"SGLIDX\0");
        assert_eq!(raw[7], 1);
        assert_eq!(raw.len() as u64, expected_idx_len_10k());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_from_past_tip_yields_zero() {
        let dir = build_10k_log("pasttip");
        let n = ChainLog::replay_from(&dir, 10_001, |_b| panic!("no block expected")).unwrap();
        assert_eq!(n, 0);
        // Missing dir / missing log → Ok(0), never an error.
        let ghost = std::env::temp_dir().join(format!("sigil-chainlog-ghost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ghost);
        assert_eq!(ChainLog::replay_from(&ghost, 5, |_b| {}).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod probe_height_tests {
    //! `probe_height_fast` — the fast skip-probe used during tail-replay catch-up
    //! (Tier 3). It was only exercised indirectly via replay. Its safety contract
    //! is "return None on ANY doubt" so a wrong/missing probe can never change
    //! which blocks get applied — these tests pin every doubt path.
    use super::{probe_height_fast, PROBE_WINDOW};

    #[test]
    fn reads_the_first_height_then_stops_at_a_non_digit() {
        assert_eq!(probe_height_fast(br#"{"header":{"version":0,"height":12345,"x":1}}"#), Some(12345));
        assert_eq!(probe_height_fast(br#""height":42}"#), Some(42), "closing brace terminates");
        assert_eq!(probe_height_fast(br#""height":7,"next":1"#), Some(7), "comma terminates");
        assert_eq!(probe_height_fast(br#""height":0,"#), Some(0), "zero is a valid height");
    }

    #[test]
    fn none_when_key_absent_or_at_height_only() {
        assert_eq!(probe_height_fast(b"no height key present"), None);
        // `"at_height":` must NOT match — the key requires a quote immediately
        // before `height`, which `at_height` lacks (the docstring's invariant).
        assert_eq!(probe_height_fast(br#"{"at_height":99}"#), None);
        assert_eq!(probe_height_fast(b""), None);
    }

    #[test]
    fn none_when_no_digits_follow_the_key() {
        assert_eq!(probe_height_fast(br#""height":abc"#), None, "non-digit right after key");
        assert_eq!(probe_height_fast(br#""height":"#), None, "key at end, no value");
    }

    #[test]
    fn refuses_a_digit_run_cut_off_by_the_probe_window() {
        // Place `"height":` so its digits run right up to the PROBE_WINDOW edge
        // with NO terminator inside the window. The probe must REFUSE (None)
        // rather than return a truncated, too-small height that could skip a
        // block we must apply — the core safety guard.
        let key = b"\"height\":";
        let pad = PROBE_WINDOW - key.len() - 3; // leaves exactly 3 digits before the edge
        let mut buf = vec![b' '; pad];
        buf.extend_from_slice(key);
        buf.extend_from_slice(b"123"); // these 3 digits sit at the very window edge
        buf.extend_from_slice(b"456789,"); // the real terminator lives PAST the window
        assert_eq!(buf.len() > PROBE_WINDOW, true);
        assert_eq!(
            probe_height_fast(&buf),
            None,
            "a digit run severed by the window edge must be refused, not truncated"
        );
        // Sanity: the SAME content with the terminator inside the window parses.
        let mut ok = vec![b' '; pad];
        ok.extend_from_slice(key);
        ok.extend_from_slice(b"12,"); // terminator (comma) is inside the window
        assert_eq!(probe_height_fast(&ok), Some(12));
    }
}
