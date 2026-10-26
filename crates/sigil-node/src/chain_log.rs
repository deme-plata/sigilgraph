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
//!
//! 2026-10-22: the `offsets` index is persisted as `chain.off` (see [`OFF_MAGIC`]), so a
//! restart reads it instead of walking every record — measured 78.5 min for a 1 TB log of
//! private-payment blocks before it.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::block::Block;

// ── raw-record cache (2026-09-13, rocky-bps100-0912) ──────────────────────────
// A block was encoded (msgpack + zstd) TWICE on the producer: once at mint for gossip,
// once again at settle for the chain log — and a received block, which arrived AS the
// encoded record, was re-encoded at settle too. MEASURED at 110 blk/s: zstd 7% +
// encode 6% of the produce loop. Remember the bytes by block hash at the point they
// already exist; the settle path takes them back. Bounded: past `RAW_CACHE_CAP` entries
// the cache is simply cleared (a miss only costs the encode it used to cost anyway).
static RAW_CACHE: std::sync::Mutex<Option<std::collections::HashMap<sigil_header::BlockHash, Vec<u8>>>> =
    std::sync::Mutex::new(None);
const RAW_CACHE_CAP: usize = 16_384;

/// Keep the encoded record for `hash` until the block settles.
pub fn remember_raw(hash: sigil_header::BlockHash, bytes: Vec<u8>) {
    if let Ok(mut g) = RAW_CACHE.lock() {
        let m = g.get_or_insert_with(std::collections::HashMap::new);
        if m.len() >= RAW_CACHE_CAP {
            m.clear();
        }
        m.insert(hash, bytes);
    }
}

/// The encoded record remembered for `hash`, if still cached (removed on take).
pub fn take_raw(hash: &sigil_header::BlockHash) -> Option<Vec<u8>> {
    RAW_CACHE.lock().ok().and_then(|mut g| g.as_mut().and_then(|m| m.remove(hash)))
}

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

/// `chain.off` — the DENSE persisted copy of the in-RAM `offsets` (2026-10-22).
///
/// Without it every restart re-derives `offsets` by walking the whole log, one disk seek per
/// record: measured 78.5 min for a 1 TB log of 0.92 MB private-payment blocks (1,084,864
/// records, 4.3 ms each), 22 min at 250 GB, 6 min at 50 GB (chronos lane
/// sigil-chronos-g3-1022). Layout: 8-byte header (`b"SGLOFF\0"` + version) followed by one
/// `u64` LE per record — the byte offset of record `i` in `chain.log` (8 B/record).
///
/// Written right AFTER each record is flushed to the log, so it can only ever LAG the log (a
/// crash between the two writes, or an older binary appending without it): `open` scans just
/// the records past its last entry. Trusted only after it checks out against the log —
/// strictly increasing from 0, every entry inside the file, and [`OFF_SAMPLES`] sampled
/// entries plus the last each sit on a real record boundary (`offset + 4 + len` equals the
/// next entry) with a readable record header. Anything else → the full scan, and the file is
/// rewritten. A stale `chain.off` costs time, never correctness.
const OFF_MAGIC: [u8; 7] = *b"SGLOFF\0";
const OFF_VERSION: u8 = 1;
const OFF_HEADER_LEN: usize = 8;
const OFF_SAMPLES: usize = 64;
/// The most a torn last record may leave dangling past the last complete one before `open`
/// refuses to cut it: far above any real record, so anything larger is not a crash mid-append
/// but a corrupt length prefix inside the log — cutting there would silently drop real blocks.
const MAX_TORN_TAIL: u64 = 64 << 20;
/// The words `open` uses when it REFUSES a log (more than [`MAX_TORN_TAIL`] dangling at a named
/// offset). sigil-top recognises a damaged block file by exactly this (10.7.3) — one string,
/// shared, so the refusal and the recovery cannot drift apart. A torn tail at or under the
/// limit is cut by `open` itself and never produces it.
pub const OPEN_REFUSED_CORRUPT: &str = "do not form a record";

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

/// ── RECORD CODEC ────────────────────────────────────────────────────────────────
///
/// The codec — `[0xB5][version][height u64 LE][zstd(MessagePack(Block))]`, the trained v2
/// dictionary, the legacy-JSON probe — lives in the `sigil-record` crate since 2026-09-14,
/// because the light client (`sigil-top`, pure-Rust zstd, Windows-cross) must read the very
/// same records off TOPIC_BLOCKS, and it could not while the codec was locked inside this
/// binary. The design notes (why MessagePack and NOT bincode: bincode cannot decode the
/// internally-tagged `SigilEvent`, and stores field order only) are on that crate's docs.
/// The names below are kept so every reader in this binary — and the tests — are unchanged.
pub(crate) use sigil_record::{
    probe_height_fast, v2_compress, v2_decompress, CHAINLOG_DICT_V2, PROBE_WINDOW, REC_MAGIC,
    REC_VERSION_V1, REC_VERSION_V2, REC_ZSTD_LEVEL,
};
#[allow(dead_code)]
pub(crate) const REC_V1_HEADER: usize = sigil_record::REC_HEADER;

/// Encode a block into a record payload. **The single encoder** — every writer goes through
/// here so a format change can never be applied to only some call sites.
pub fn encode_record(block: &Block) -> std::io::Result<Vec<u8>> {
    sigil_record::encode_record(block.header.height, block)
}

/// Decode a record payload written in EITHER format. **The single decoder.**
///
/// Returns `None` rather than panicking on a torn or unknown record: every caller already
/// treats a failed decode as "stop the scan here", which is the correct behaviour for an
/// append-only log whose tail may be a partial write after a crash.
pub fn decode_record(payload: &[u8]) -> Option<Block> {
    let block = sigil_record::decode_record(payload);
    if block.is_none() {
        warn_if_newer_format(payload);
    }
    block
}

/// 2026-10-19: a record in a body format NEWER than this binary is not "the end of the log" —
/// it is a chain this node can no longer follow. Say so, plainly and at most once a minute,
/// wherever the record came from (gossip, backfill, replay); the caller's handling is unchanged.
fn warn_if_newer_format(payload: &[u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    let Some(v) = sigil_record::unsupported_record_version(payload) else { return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let last = LAST_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 60_000 && LAST_MS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        eprintln!(
            "⛔ block record in body format v{v}, newer than this binary understands (max v{}) — \
             this node must be UPGRADED to keep following the chain",
            sigil_record::REC_VERSION_V2
        );
    }
}

/// Height of a record, from its framing, without decompressing the body.
///
/// `pub(crate)` as of 2026-08-27 because `serve_read` had its OWN copy that only
/// understood the legacy JSON form — see that module for what it cost. There is one
/// record format and there must be one reader of it.
pub(crate) fn probe_height(payload: &[u8]) -> Option<u64> {
    sigil_record::probe_height(payload)
}

/// How far below the wanted height a seek hint may land: the readers walk forward from it, as
/// they did from a sparse `chain.idx` entry (at most [`IDX_EVERY`] below), so allow the same.
const SEEK_HINT_REACH: u64 = 4 * IDX_EVERY;

/// The height of the record that starts at `off`, from its leading bytes only.
fn height_at(log: &mut File, off: u64, log_len: u64) -> Option<u64> {
    if off.checked_add(4)? > log_len {
        return None;
    }
    log.seek(SeekFrom::Start(off)).ok()?;
    let mut lb = [0u8; 4];
    log.read_exact(&mut lb).ok()?;
    let len = u32::from_le_bytes(lb) as u64;
    if len == 0 || off + 4 + len > log_len {
        return None;
    }
    let mut head = vec![0u8; (len as usize).min(PROBE_WINDOW)];
    log.read_exact(&mut head).ok()?;
    probe_height(&head)
}

/// 2026-10-26 (10.7.3): where to start reading for `height`, from the DENSE record offsets
/// (`chain.off`, or `ChainLog::offsets` in RAM) instead of the sparse `chain.idx` + a forward
/// walk that decoded every record it passed. MEASURED on a 1 TB log of 0.92 MB blocks:
/// `get_by_height` p50 6.9 s / max 12.8 s, while a read at a known offset took 57 ms.
///
/// Entry `i` is the i-th RECORD, which is height `i` only for a log that starts at genesis; a
/// node that began from a snapshot starts at some height `base`, so `height - base` is tried
/// first. Every hint is checked by probing the height at that offset: the result is either a
/// record AT `height`, or one at most [`SEEK_HINT_REACH`] below it (callers walk forward).
/// `None` → no usable hint; callers fall back to `chain.idx`. A stale or misaligned table
/// (rewound logs, the g2 +1077 anomaly) costs time, never correctness.
fn seek_hint(log: &mut File, log_len: u64, entries: u64, mut entry: impl FnMut(u64) -> Option<u64>, height: u64) -> Option<u64> {
    if entries == 0 {
        return None;
    }
    let base = height_at(log, entry(0)?, log_len)?;
    let mut tried = None;
    for i in [height.checked_sub(base), Some(height)].into_iter().flatten() {
        if i >= entries || tried == Some(i) {
            continue;
        }
        tried = Some(i);
        let Some(off) = entry(i) else { continue };
        match height_at(log, off, log_len) {
            Some(h) if h == height || (h < height && height - h <= SEEK_HINT_REACH) => return Some(off),
            _ => {}
        }
    }
    None
}

/// [`seek_hint`] from the `chain.off` file beside a `chain.log` — for the read-only serve path,
/// which holds no `ChainLog` (see `serve_read`). Two small reads of `chain.off`, two record
/// probes; the file being appended concurrently is fine (a missing or partial entry → `None`).
pub(crate) fn off_seek(dir: &Path, height: u64) -> Option<u64> {
    let mut off = File::open(dir.join("chain.off")).ok()?;
    let mut hdr = [0u8; OFF_HEADER_LEN];
    off.read_exact(&mut hdr).ok()?;
    if hdr[..7] != OFF_MAGIC || hdr[7] != OFF_VERSION {
        return None;
    }
    let entries = off.metadata().ok()?.len().saturating_sub(OFF_HEADER_LEN as u64) / 8;
    let mut log = File::open(dir.join("chain.log")).ok()?;
    let log_len = log.metadata().ok()?.len();
    let entry = |i: u64| -> Option<u64> {
        let mut b = [0u8; 8];
        off.seek(SeekFrom::Start(OFF_HEADER_LEN as u64 + 8 * i)).ok()?;
        off.read_exact(&mut b).ok()?;
        Some(u64::from_le_bytes(b))
    };
    seek_hint(&mut log, log_len, entries, entry, height)
}

pub struct ChainLog {
    path: PathBuf,
    writer: BufWriter<File>,
    /// `offsets[h]` = byte offset of block `h`'s record in the log.
    offsets: Vec<u64>,
    bytes_len: u64,
    /// Best-effort append handle for `chain.idx`. `None` = index writes are
    /// disabled (open/IO error) — reads then fall back to filtered scans.
    idx_writer: Option<BufWriter<File>>,
    /// Best-effort append handle for `chain.off`. `None` = disabled after an IO error; the
    /// next `open` then scans the records it is missing.
    off_writer: Option<BufWriter<File>>,
    /// How the last `open` got its offsets: records taken from `chain.off`, records scanned.
    opened_from_off: u64,
    opened_by_scan: u64,
}

impl ChainLog {
    /// Open (creating if absent). The offset index comes from `chain.off` when it checks out
    /// against the log (see [`OFF_MAGIC`]); only the records past what it covers are scanned —
    /// all of them on the first open after an upgrade, or when `chain.off` is missing or
    /// rejected (it is then rewritten). A torn last record (a crash mid-append) is cut off and
    /// kept beside the log as `chain.log.torn-<offset>`, so later appends land where the index
    /// says they do.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("chain.log");
        let off_path = Self::off_path_for(&path);
        let mut offsets = Vec::new();
        let mut bytes_len = 0u64;
        let mut trusted = 0usize; // leading entries of `offsets` that chain.off already holds
        if path.exists() {
            let t0 = std::time::Instant::now();
            let file_len = std::fs::metadata(&path)?.len();
            let mut f = File::open(&path)?;
            let mut start = 0u64;
            if let Some((persisted, end)) = Self::load_offsets(&off_path, &mut f, file_len) {
                trusted = persisted.len();
                offsets = persisted;
                start = end;
            }
            let (end, torn) = Self::scan_offsets(f, start, file_len, &mut offsets)?;
            bytes_len = end;
            if torn {
                Self::cut_torn_tail(&path, end, file_len, MAX_TORN_TAIL)?;
            }
            if !offsets.is_empty() {
                eprintln!(
                    "chain.log: {} records — {} from chain.off, {} scanned ({:.1} s)",
                    offsets.len(), trusted, offsets.len() - trusted, t0.elapsed().as_secs_f64()
                );
            }
        }
        let writer = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path)?);
        let idx_writer = Self::open_idx_writer(&Self::idx_path_for(&path), offsets.is_empty());
        let off_writer = Self::open_off_writer(&off_path, &offsets, trusted);
        let (opened_from_off, opened_by_scan) = (trusted as u64, (offsets.len() - trusted) as u64);
        Ok(Self { path, writer, offsets, bytes_len, idx_writer, off_writer, opened_from_off, opened_by_scan })
    }

    /// How the last `open` got its offsets: (records taken from `chain.off`, records scanned).
    pub fn open_stats(&self) -> (u64, u64) {
        (self.opened_from_off, self.opened_by_scan)
    }

    /// Walk the records from `start` to EOF, pushing each record's offset. Returns the end of
    /// the last COMPLETE record and whether the file ends in a torn one — its length prefix or
    /// its body runs past EOF. (The old scan relied on `seek_relative` failing there; it does
    /// not fail past EOF, so a torn record was counted and later appends landed after its
    /// garbage, misaligned with `offsets` — found by the 2026-10-18 chronos torn-tail probe.)
    fn scan_offsets(f: File, start: u64, file_len: u64, offsets: &mut Vec<u64>) -> std::io::Result<(u64, bool)> {
        let mut r = BufReader::new(f);
        r.seek(SeekFrom::Start(start))?;
        let mut pos = start;
        loop {
            if pos == file_len {
                return Ok((pos, false));
            }
            if pos + 4 > file_len {
                return Ok((pos, true));
            }
            let mut lb = [0u8; 4];
            r.read_exact(&mut lb)?;
            let rec = u32::from_le_bytes(lb) as u64;
            if pos + 4 + rec > file_len {
                return Ok((pos, true));
            }
            offsets.push(pos);
            pos += 4 + rec;
            // seek_relative (not seek(SeekFrom::Start)) — a BufReader's Seek impl
            // unconditionally discards its internal buffer on every call, turning
            // this scan into one raw unbuffered syscall PER RECORD (each pulling a
            // fresh OS page just to read the next 4-byte length prefix). At tens of
            // millions of records that's tens of millions of syscalls and hundreds
            // of GB of redundant reads — the actual cause of a multi-hour open() on
            // a large chain.log. seek_relative stays inside the buffer whenever the
            // skip distance fits (always true here: records are ~hundreds of bytes,
            // the buffer is 8 KB), so the common case costs no syscall at all.
            r.seek_relative(rec as i64)?;
        }
    }

    /// Cut a torn last record off the log, keeping its bytes beside it as evidence. `end` is
    /// the end of the last record that fits entirely inside the file. Refuses (an error, so the
    /// node does not start on it) when more than `max_cut` bytes dangle: that is a corrupt
    /// length prefix inside the log, not a crash, and cutting would drop real blocks.
    fn cut_torn_tail(path: &Path, end: u64, file_len: u64, max_cut: u64) -> std::io::Result<()> {
        let dangling = file_len - end;
        if dangling > max_cut {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "chain.log: {dangling} bytes after the last complete record at offset {end} {OPEN_REFUSED_CORRUPT} — a \
                     corrupt length prefix, not a torn append; not cutting. The blocks are on the network: set the data \
                     folder aside and re-sync (sigil-top does this itself on a user node)"
                ),
            ));
        }
        let keep = path.with_file_name(format!("chain.log.torn-{end}"));
        let mut f = OpenOptions::new().read(true).write(true).open(path)?;
        f.seek(SeekFrom::Start(end))?;
        std::io::copy(&mut (&mut f).take(dangling), &mut File::create(&keep)?)?;
        f.set_len(end)?;
        f.sync_all()?;
        eprintln!(
            "⚠ chain.log ended in a torn record: {dangling} bytes after the last complete one (offset {end}) cut off, kept as {}",
            keep.display()
        );
        Ok(())
    }

    /// `chain.off` lives next to `chain.log`.
    fn off_path_for(log_path: &Path) -> PathBuf {
        log_path.with_file_name("chain.off")
    }

    /// `chain.off`'s offsets and the end of the last record they cover — if, and only if, the
    /// file checks out against the log (see [`OFF_MAGIC`]). `None` → scan from byte 0.
    fn load_offsets(off_path: &Path, log: &mut File, file_len: u64) -> Option<(Vec<u64>, u64)> {
        let raw = std::fs::read(off_path).ok()?;
        if raw.len() < OFF_HEADER_LEN || raw[..7] != OFF_MAGIC || raw[7] != OFF_VERSION {
            return None;
        }
        // A crash mid-entry leaves a partial trailing entry: chunks_exact drops it.
        let offsets: Vec<u64> =
            raw[OFF_HEADER_LEN..].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        let Some(&last) = offsets.last() else { return Some((offsets, 0)) };
        if offsets[0] != 0 || last >= file_len || !offsets.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        let n = offsets.len();
        let mut picks: Vec<usize> = (0..OFF_SAMPLES).map(|k| k * (n - 1) / OFF_SAMPLES).collect();
        picks.push(n - 1);
        picks.dedup();
        let mut end_of_last = 0;
        for i in picks {
            let end = Self::record_end_if_whole(log, offsets[i], file_len)?;
            if i + 1 < n && end != offsets[i + 1] {
                return None;
            }
            end_of_last = end;
        }
        Some((offsets, end_of_last))
    }

    /// The end of the record at `off`, if a whole record with a readable header starts there.
    fn record_end_if_whole(log: &mut File, off: u64, file_len: u64) -> Option<u64> {
        log.seek(SeekFrom::Start(off)).ok()?;
        let mut lb = [0u8; 4];
        log.read_exact(&mut lb).ok()?;
        let len = u32::from_le_bytes(lb) as u64;
        let end = off + 4 + len;
        if len == 0 || end > file_len {
            return None;
        }
        let mut head = vec![0u8; (len as usize).min(PROBE_WINDOW)];
        log.read_exact(&mut head).ok()?;
        probe_height(&head)?;
        Some(end)
    }

    /// Bring `chain.off` up to date with `offsets` and keep it open for appends. `trusted` =
    /// how many leading entries the file already holds (checked by [`Self::load_offsets`]);
    /// 0 = missing or rejected, so it is rewritten whole (tmp + rename). Best-effort: on an IO
    /// error the file is removed and the next `open` scans, exactly as before it existed.
    fn open_off_writer(off_path: &Path, offsets: &[u64], trusted: usize) -> Option<BufWriter<File>> {
        let res = (|| -> std::io::Result<BufWriter<File>> {
            if trusted == 0 {
                let tmp = off_path.with_extension("off.tmp");
                let mut w = BufWriter::new(File::create(&tmp)?);
                let mut hdr = [0u8; OFF_HEADER_LEN];
                hdr[..7].copy_from_slice(&OFF_MAGIC);
                hdr[7] = OFF_VERSION;
                w.write_all(&hdr)?;
                for o in offsets {
                    w.write_all(&o.to_le_bytes())?;
                }
                w.flush()?;
                w.get_ref().sync_all()?;
                drop(w);
                std::fs::rename(&tmp, off_path)?;
            } else {
                // Drop a partial trailing entry, then add what the scan found past the index.
                OpenOptions::new().write(true).open(off_path)?.set_len((OFF_HEADER_LEN + 8 * trusted) as u64)?;
                let mut w = BufWriter::new(OpenOptions::new().append(true).open(off_path)?);
                for o in &offsets[trusted..] {
                    w.write_all(&o.to_le_bytes())?;
                }
                w.flush()?;
                return Ok(w);
            }
            Ok(BufWriter::new(OpenOptions::new().append(true).open(off_path)?))
        })();
        match res {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("⚠ chain.off disabled ({e}) — the next open scans chain.log");
                let _ = std::fs::remove_file(off_path);
                None
            }
        }
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
        let bytes = encode_record(block)?;
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
            if let Some(h) = probe_height(bytes) { let p = HeightProbe { header: HeaderHeightProbe { height: h } };
                let off = self.bytes_len;
                self.write_idx_entry(p.header.height, off);
            }
        }
        let off = self.bytes_len;
        self.offsets.push(off);
        self.bytes_len += 4 + bytes.len() as u64;
        // chain.off AFTER the record is flushed: the index may lag the log, never lead it.
        if let Some(w) = self.off_writer.as_mut() {
            if w.write_all(&off.to_le_bytes()).and_then(|_| w.flush()).is_err() {
                self.off_writer = None;
            }
        }
        Ok(())
    }

    /// Read block at `height` from disk (for serving backfill of pruned blocks).
    ///
    /// 2026-08-20: `offsets[i]` is populated by `open()`'s startup scan as
    /// literally "the i-th record found in the file" — it does NOT verify
    /// that record's own `header.height` field equals `i`. Every append
    /// during a single continuous run keeps this 1:1 by construction (`i`
    /// only ever grows in lockstep with real height), so a long-lived
    /// process's own `offsets` is always correct — but if the file's
    /// history ever contains a stretch that isn't strictly one record per
    /// sequential height (found live on Epsilon: a fixed +1077 offset
    /// starting somewhere in the 240k-300k range, almost certainly from a
    /// historical bulk-import event), a FRESH `open()` after a restart
    /// reconstructs an `offsets` that's silently misaligned with real
    /// height from that point on. This is why the boot-time snapshot
    /// continuity check (`main.rs`) kept reporting a hash mismatch and
    /// falling back to a full replay on every single restart — it was
    /// comparing against the WRONG record, not a genuinely corrupt one
    /// (confirmed live: the record `get_by_height` below actually finds at
    /// the snapshot's claimed height has the EXACT hash the snapshot
    /// expects). `get`/`get_range` are UNCHANGED here (any live caller of
    /// those already gets self-corrected height≥request rejections at
    /// apply time — see the module doc — so this doesn't fix them; it adds
    /// a separate, genuinely height-verified path for callers that need
    /// the guarantee `get()` was documented to provide but doesn't fully
    /// deliver post-restart).
    pub fn get(&self, height: u64) -> Option<Block> {
        let off = *self.offsets.get(height as usize)?;
        let mut f = File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut lb = [0u8; 4];
        f.read_exact(&mut lb).ok()?;
        let n = u32::from_le_bytes(lb) as usize;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf).ok()?;
        decode_record(&buf)
    }

    /// The REAL height of the last record on disk — i.e. `get(N-1)`'s own
    /// `header.height` field, not `N-1` itself. `offsets[i]` always points
    /// at the true i-th physical record in the file (that part of `open()`'s
    /// scan is fine); the bug documented on [`height`](Self::height)/
    /// [`get`](Self::get) is trusting `i == that record's real height`,
    /// which the +1077 historical anomaly breaks. So the LAST offset still
    /// correctly points at the actual last record — this just reads that
    /// one record and reports what height it really claims, instead of
    /// reporting the record count. `None` on an empty log or a read/decode
    /// failure (callers should treat that as "can't confirm — don't trust
    /// it", not as height 0).
    pub fn tip_real_height(&self) -> Option<u64> {
        let off = *self.offsets.last()?;
        let mut f = File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut lb = [0u8; 4];
        f.read_exact(&mut lb).ok()?;
        let n = u32::from_le_bytes(lb) as usize;
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf).ok()?;
        let block: Block = decode_record(&buf)?;
        Some(block.header.height)
    }

    /// Like [`get`](Self::get), but VERIFIES the returned block's own
    /// `header.height` actually equals `height` — immune to the `offsets`
    /// misalignment documented on `get()` above. Reuses the same
    /// `chain.idx`-validated seek `replay_from` already relies on (a
    /// sparse, self-healing height→offset index, checked every
    /// [`IDX_EVERY`] blocks — NOT the raw scan-built `offsets` array), then
    /// scans forward a SHORT, bounded distance re-reading each record's
    /// real height until it finds an exact match, passes it (gap — no such
    /// height stored), or hits the bound. Cost: one bounded disk scan
    /// (≤`IDX_EVERY`-ish records in the common case), not a proportional
    /// fraction of the whole file — safe to call from a hot path, though
    /// today's only caller is the one-time boot-time snapshot check.
    pub fn get_by_height(&self, height: u64) -> Option<Block> {
        const MAX_SCAN: u64 = IDX_EVERY * 4; // generous vs. the 512-block index stride
        let start = self.seek_start(height);
        let mut r = BufReader::new(File::open(&self.path).ok()?);
        r.seek(SeekFrom::Start(start)).ok()?;
        for _ in 0..MAX_SCAN {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() {
                return None; // EOF before reaching the target height
            }
            let n = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() {
                return None; // torn tail record
            }
            // 10.7.3: pass records below the target by their framed height — decoding each one
            // was most of the 6.9 s a lookup took on a 1 TB log.
            match probe_height(&buf) {
                Some(h) if h < height => continue,
                Some(h) if h > height => return None,
                _ => {}
            }
            let block: Block = match decode_record(&buf).ok_or(()) {
                Ok(b) => b,
                Err(_) => continue, // shouldn't happen; keep scanning rather than abort
            };
            match block.header.height.cmp(&height) {
                std::cmp::Ordering::Equal => return Some(block),
                std::cmp::Ordering::Greater => return None, // stepped past it — no such height
                std::cmp::Ordering::Less => continue,
            }
        }
        None
    }

    /// Like [`get_range`](Self::get_range), but height-validated the same way
    /// [`get_by_height`](Self::get_by_height) is — immune to the `offsets`
    /// misalignment documented on `get()`'s doc comment.
    ///
    /// 2026-08-21: found live serving a real backfill request — a node whose
    /// `offsets` array had the same post-restart misalignment as `get()`
    /// answered `[N..]` with **zero headers** for a range it genuinely had
    /// on disk (proven: the requester's OWN sync continued past `N` on this
    /// exact node earlier, and a sibling node served the identical range
    /// fine). This is exactly the "wasted/failed backfill-serve retries"
    /// blast radius predicted when `get()` was fixed but `get_range()` was
    /// deliberately left alone — confirmed, not hypothetical, once observed
    /// against a peer stuck re-requesting the same range for minutes.
    ///
    /// Reuses `idx_seek_offset` for a validated starting position (may land
    /// slightly BEFORE `from`, since `chain.idx` is sparse — every
    /// `IDX_EVERY` blocks — so this skips forward re-reading each record's
    /// real height until reaching `from`, then collects through `to` exactly
    /// like the original). Still one seek + one sequential read; the skip
    /// phase is bounded by the same `IDX_EVERY` stride `get_by_height` scans.
    pub fn get_range_by_height(&self, from: u64, to: u64) -> Vec<Block> {
        let mut out = Vec::new();
        let start = self.seek_start(from);
        let Ok(f) = File::open(&self.path) else { return out };
        let mut r = BufReader::new(f);
        if r.seek(SeekFrom::Start(start)).is_err() { return out; }
        loop {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() { break; } // EOF
            let n = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() { break; } // torn tail record
            match probe_height(&buf) {
                Some(h) if h < from => continue, // 10.7.3: skip by framed height, no decode
                Some(h) if h > to => break,
                _ => {}
            }
            let block: Block = match decode_record(&buf).ok_or(()) {
                Ok(b) => b,
                Err(_) => continue, // shouldn't happen; keep scanning rather than abort
            };
            if block.header.height < from { continue; } // skip-forward from a sparse idx landing
            if block.header.height > to { break; }
            out.push(block);
        }
        out
    }

    /// Where the height-verified readers start: the dense in-RAM `offsets`, checked by
    /// [`seek_hint`]; else the sparse `chain.idx` as before; else the start of the log.
    fn seek_start(&self, height: u64) -> u64 {
        let offs = &self.offsets;
        let hint = File::open(&self.path).ok().and_then(|mut f| {
            let len = f.metadata().ok()?.len();
            seek_hint(&mut f, len, offs.len() as u64, |i| offs.get(i as usize).copied(), height)
        });
        hint.or_else(|| self.path.parent().and_then(|dir| Self::idx_seek_offset(dir, height, &self.path)))
            .unwrap_or(0)
    }

    /// Read a contiguous height range `[from..=to]` from disk with ONE file open +
    /// sequential read (vs `get()` which opens the file per height — 8192 opens/chunk
    /// was a serve bottleneck). Stops at the end of the log.
    ///
    /// 2026-08-21: `offsets`-indexed, so it carries the SAME position-vs-
    /// real-height fragility as `get()` (see that method's doc comment) —
    /// confirmed live to silently return zero blocks for a range that
    /// genuinely exists on disk. Prefer [`get_range_by_height`] for any new
    /// caller; kept here unmodified for existing call sites until they're
    /// migrated.
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
            match decode_record(&buf).ok_or(()) {
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
            match decode_record(&buf).ok_or(()) {
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

    /// Like [`replay_from`](Self::replay_from), but bounded ABOVE by
    /// `to_height` (INCLUSIVE): stops at the first record past the window
    /// instead of scanning to EOF. Same `chain.idx` seek, same probe-skip
    /// fast path, same torn-tail tolerance; read-only (no writer handle, no
    /// idx rebuild), so it is safe to run against a log the producer is
    /// actively appending to.
    ///
    /// WHY THIS EXISTS (2026-08-26 incident): the search indexer caught up
    /// with an UNBOUNDED `replay_from`, and only wrote its progress file
    /// after that single pass returned. With a ~209k-block backlog the pass
    /// never returned before the process hit its cgroup memory ceiling, so
    /// progress was never checkpointed and every restart replayed the exact
    /// same doomed range — a livelock that starved block production for
    /// hours. A bounded range is what makes the catch-up resumable, so a
    /// crash costs one batch instead of all progress.
    pub fn replay_range(
        dir: &std::path::Path,
        from_height: u64,
        to_height: u64,
        mut f: impl FnMut(Block),
    ) -> Result<u64, String> {
        if to_height < from_height {
            return Ok(0);
        }
        let log_path = dir.join("chain.log");
        if !log_path.exists() {
            return Ok(0);
        }
        // `unwrap_or(0)` matches `get_range_by_height`: an unusable index just
        // means "start at byte 0 and let the height filter do the work" —
        // correct, only slower. Deliberately does NOT fall back to
        // `full_scan_rebuild`: that path rewrites chain.idx, and this reader
        // must stay side-effect-free.
        let start = Self::idx_seek_offset(dir, from_height, &log_path).unwrap_or(0);
        Self::scan_range(&log_path, start, from_height, to_height, &mut f)
    }

    /// Scan `[from_height, to_height]` from byte `start`, calling `f` for each
    /// block inside the window and stopping at the first record above it.
    /// Records below the window are skipped via the same head-probe used by
    /// [`scan_filtered`](Self::scan_filtered) — microseconds instead of a
    /// ~0.5 ms full decode each.
    fn scan_range(
        log_path: &Path,
        start: u64,
        from_height: u64,
        to_height: u64,
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
            let head_len = len.min(PROBE_WINDOW);
            if r.read_exact(&mut head[..head_len]).is_err() {
                break; // torn tail record
            }
            match probe_height(&head[..head_len]) {
                // Below the window: seek over the payload, never decode it.
                Some(h) if h < from_height => {
                    if r.seek_relative((len - head_len) as i64).is_err() {
                        break;
                    }
                    continue;
                }
                // Past the window: the log is append-ordered by height, so
                // nothing further can be in range. Same convention as
                // `get_range_by_height`.
                Some(h) if h > to_height => break,
                // Probe unsure — fall through to the authoritative decode.
                _ => {}
            }
            let mut buf = vec![0u8; len];
            buf[..head_len].copy_from_slice(&head[..head_len]);
            if r.read_exact(&mut buf[head_len..]).is_err() {
                break; // torn tail record
            }
            match decode_record(&buf).ok_or(()) {
                Ok(b) => {
                    // The decoded header is always authoritative, never the probe.
                    if b.header.height > to_height {
                        break;
                    }
                    if b.header.height >= from_height {
                        f(b);
                        n += 1;
                    }
                }
                Err(_) => break,
            }
        }
        Ok(n)
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
        let probe = HeightProbe { header: HeaderHeightProbe { height: probe_height(&buf)? } };
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
                match probe_height(&head[..head_len]) {
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
                        match decode_record(&buf).ok_or(()) {
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
            match decode_record(&buf).ok_or(()) {
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
                if let Some(h) = probe_height(&head[..head_len]) {
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
                match decode_record(&buf).ok_or(()) {
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

    // ── chain.off (2026-10-22) ──────────────────────────────────────────────────────

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sigil-chainoff-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// What every open derived before chain.off existed: walk each record from byte 0.
    pub(super) fn offsets_by_full_scan(dir: &Path) -> Vec<u64> {
        let path = dir.join("chain.log");
        let len = std::fs::metadata(&path).unwrap().len();
        let mut v = Vec::new();
        let (end, torn) = ChainLog::scan_offsets(File::open(&path).unwrap(), 0, len, &mut v).unwrap();
        assert!(!torn && end == len, "the fixture log must be whole");
        v
    }

    fn off_entries(dir: &Path) -> u64 {
        (std::fs::metadata(dir.join("chain.off")).unwrap().len() - OFF_HEADER_LEN as u64) / 8
    }

    fn write_log(dir: &Path, blocks: &[Block]) {
        let mut log = ChainLog::open(dir).unwrap();
        for b in blocks {
            log.append(b).unwrap();
        }
    }

    #[test]
    fn chain_off_tracks_every_append_and_a_reopen_trusts_it() {
        let dir = fresh_dir("trust");
        let blocks = crate::block::__test_chain(305);
        write_log(&dir, &blocks[..300]);
        assert_eq!(off_entries(&dir), 300, "one entry per appended record");
        let mut log = ChainLog::open(&dir).unwrap();
        assert_eq!(log.open_stats(), (300, 0), "a whole chain.off means nothing is scanned");
        assert_eq!(log.offsets, offsets_by_full_scan(&dir), "chain.off == what the full scan derives");
        for b in &blocks[300..] {
            log.append(b).unwrap();
        }
        drop(log);
        assert_eq!(off_entries(&dir), 305);
        let log = ChainLog::open(&dir).unwrap();
        assert_eq!(log.open_stats(), (305, 0));
        assert_eq!(log.get(304).unwrap().header.height, 305);
        assert_eq!(log.offsets, offsets_by_full_scan(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chain_off_behind_the_log_is_caught_up_by_a_short_scan() {
        // A crash between a record and its chain.off entry, or an older binary appending.
        let dir = fresh_dir("behind");
        write_log(&dir, &crate::block::__test_chain(300));
        let off = dir.join("chain.off");
        OpenOptions::new().write(true).open(&off).unwrap().set_len((OFF_HEADER_LEN + 8 * 200 + 3) as u64).unwrap();
        let log = ChainLog::open(&dir).unwrap();
        assert_eq!(log.open_stats(), (200, 100), "only the 100 records past chain.off are scanned");
        assert_eq!(log.offsets, offsets_by_full_scan(&dir));
        drop(log);
        assert_eq!(off_entries(&dir), 300, "chain.off is whole again");
        assert_eq!((std::fs::metadata(&off).unwrap().len() - OFF_HEADER_LEN as u64) % 8, 0, "the torn entry is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_garbled_chain_off_falls_back_to_the_full_scan() {
        let dir = fresh_dir("garbled");
        write_log(&dir, &crate::block::__test_chain(300));
        let off = dir.join("chain.off");
        let good = std::fs::read(&off).unwrap();
        let at = |i: usize| OFF_HEADER_LEN + 8 * i;
        let set = |b: &mut Vec<u8>, i: usize, v: u64| b[at(i)..at(i) + 8].copy_from_slice(&v.to_le_bytes());
        let get = |b: &[u8], i: usize| u64::from_le_bytes(b[at(i)..at(i) + 8].try_into().unwrap());
        let mut swapped = good.clone();
        let (a, b) = (get(&good, 10), get(&good, 11));
        set(&mut swapped, 10, b);
        set(&mut swapped, 11, a);
        let mut beyond = good.clone();
        set(&mut beyond, 299, u64::MAX);
        let mut off_boundary = good.clone();
        set(&mut off_boundary, 299, get(&good, 299) + 1);
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xFF;
        let expect = offsets_by_full_scan(&dir);
        for (what, content) in [
            ("missing", None),
            ("bad magic", Some(bad_magic)),
            ("two entries swapped", Some(swapped)),
            ("an entry beyond the file", Some(beyond)),
            ("an entry off a record boundary", Some(off_boundary)),
        ] {
            match &content {
                Some(c) => std::fs::write(&off, c).unwrap(),
                None => std::fs::remove_file(&off).unwrap(),
            }
            let log = ChainLog::open(&dir).unwrap();
            assert_eq!(log.open_stats(), (0, 300), "{what}: rejected → full scan");
            assert_eq!(log.offsets, expect, "{what}: offsets still right");
            drop(log);
            assert_eq!(std::fs::read(&off).unwrap(), good, "{what}: chain.off rewritten whole");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chain_off_from_a_different_log_is_rejected() {
        // The same 300 blocks re-encoded in the older v1 format: other record sizes, so other
        // boundaries — what a swapped-in repaired or recompressed log looks like to chain.off.
        let dir = fresh_dir("foreign");
        let blocks = crate::block::__test_chain(300);
        write_log(&dir, &blocks);
        let stale = std::fs::read(dir.join("chain.off")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for b in &blocks {
                log.append_bytes(&super::probe_height_tests::encode_v1_for_test(b)).unwrap();
            }
        }
        assert_ne!(std::fs::read(dir.join("chain.off")).unwrap(), stale, "the fixture must really move the boundaries");
        let expect = offsets_by_full_scan(&dir);
        std::fs::write(dir.join("chain.off"), &stale).unwrap();
        let log = ChainLog::open(&dir).unwrap();
        assert_eq!(log.open_stats(), (0, 300), "a foreign chain.off is rejected");
        assert_eq!(log.offsets, expect);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_last_record_is_cut_and_later_appends_stay_aligned() {
        let blocks = crate::block::__test_chain(101);
        let mut cut_short = 5000u32.to_le_bytes().to_vec();
        cut_short.extend([0xAB; 100]);
        for (what, junk) in [("half a length prefix", vec![0x10u8, 0x27]), ("a body cut short", cut_short)] {
            let dir = fresh_dir("torn");
            write_log(&dir, &blocks[..100]);
            let path = dir.join("chain.log");
            let whole = std::fs::metadata(&path).unwrap().len();
            OpenOptions::new().append(true).open(&path).unwrap().write_all(&junk).unwrap();
            let mut log = ChainLog::open(&dir).unwrap();
            assert_eq!(log.height(), 100, "{what}: the torn record is not counted");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), whole, "{what}: cut back to the last whole record");
            assert_eq!(std::fs::read(dir.join(format!("chain.log.torn-{whole}"))).unwrap(), junk, "{what}: the cut bytes are kept");
            log.append(&blocks[100]).unwrap();
            drop(log);
            let log = ChainLog::open(&dir).unwrap();
            assert_eq!(log.height(), 101);
            assert_eq!(log.get(100).unwrap().header.height, 101, "{what}: the next append is where the index says");
            assert_eq!(log.offsets, offsets_by_full_scan(&dir));
            assert_eq!(ChainLog::replay(&dir, |_| {}).unwrap(), 101);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_large_dangling_region_is_refused_not_cut() {
        // More than `max_cut` past the last whole record is a corrupt length prefix, not a crash.
        let dir = fresh_dir("dangling");
        write_log(&dir, &crate::block::__test_chain(10));
        let path = dir.join("chain.log");
        let whole = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(&[0xEE; 300]).unwrap();
        let err = ChainLog::cut_torn_tail(&path, whole, whole + 300, 256).expect_err("300 > 256 dangling bytes: refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), whole + 300, "nothing cut");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 10.7.3 (b): the dense table finds the record in one checked seek — for a log that does not
    /// start at genesis (`__test_chain` is heights 1..=n, so entry i holds height i + 1), and a
    /// table that LIES (shifted above the wanted height) is refused and the old path answers.
    #[test]
    fn seek_hint_finds_the_record_and_never_trusts_a_lying_table() {
        let dir = fresh_dir("seekhint");
        let blocks = crate::block::__test_chain(600);
        write_log(&dir, &blocks);
        let offs = offsets_by_full_scan(&dir);
        for h in [1u64, 2, 300, 511, 512, 513, 600] {
            assert_eq!(off_seek(&dir, h), Some(offs[(h - 1) as usize]), "chain.off seek lands on height {h}");
        }
        assert_eq!(off_seek(&dir, 601), None, "past the end: no hint");
        let log = ChainLog::open(&dir).unwrap();
        for h in [1u64, 299, 600] {
            assert_eq!(log.get_by_height(h).unwrap().header.height, h);
        }
        assert_eq!(log.get_range_by_height(510, 515).iter().map(|b| b.header.height).collect::<Vec<_>>(), (510..=515).collect::<Vec<_>>());
        drop(log);

        // A table shifted 3 records AHEAD: every hint lands above the wanted height → refused.
        let write_off = |entries: &[u64]| {
            let mut raw = Vec::from(OFF_MAGIC);
            raw.push(OFF_VERSION);
            for o in entries {
                raw.extend(o.to_le_bytes());
            }
            std::fs::write(dir.join("chain.off"), raw).unwrap();
        };
        let ahead: Vec<u64> = (0..offs.len()).map(|i| offs[(i + 3).min(offs.len() - 1)]).collect();
        write_off(&ahead);
        assert_eq!(off_seek(&dir, 300), Some(offs[299]),
            "a UNIFORM shift moves the base probe too, so the base correction lands on the true record");
        // A table that really lies: entry 0 honest, every other entry at the LAST record (height
        // 600) — each hint lands above the wanted height and is refused.
        let lying: Vec<u64> = (0..offs.len()).map(|i| if i == 0 { offs[0] } else { offs[offs.len() - 1] }).collect();
        write_off(&lying);
        assert_eq!(off_seek(&dir, 300), None, "a hint above the wanted height is never used");
        assert_eq!(read_blocks_via_log(&dir, 300), Some(300), "and the reader still answers, through chain.idx");
        // A table shifted 10 records BEHIND: lands below, within reach → used, and the walk forward finds it.
        let behind: Vec<u64> = (0..offs.len()).map(|i| offs[i.saturating_sub(10)]).collect();
        write_off(&behind);
        let hint = off_seek(&dir, 300).expect("a hint below, within reach, is used");
        assert!(hint < offs[299], "it lands below the record and the reader walks forward");
        for got in [read_blocks_via_log(&dir, 300), read_blocks_via_log(&dir, 1)] {
            assert!(got.is_some(), "the readers still answer through any table");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Open a log WITHOUT letting `open` rewrite chain.off, and read one height through it.
    fn read_blocks_via_log(dir: &Path, h: u64) -> Option<u64> {
        crate::serve_read::read_records_range(dir, h, h)
            .first()
            .and_then(|r| decode_record(r))
            .map(|b| b.header.height)
            .filter(|got| *got == h)
    }

    /// 10.7.3 (a), both sides of the 64 MiB line THROUGH `open` (sparse files, no 64 MiB written
    /// on the refuse side): at the limit the torn tail is cut and the log opens; one byte over,
    /// `open` refuses with [`OPEN_REFUSED_CORRUPT`] naming the offset — the only words sigil-top
    /// acts on, so the two paths can never both fire.
    #[test]
    fn open_cuts_at_the_64_mib_line_and_refuses_one_byte_over() {
        for (over, tag) in [(false, "at-limit"), (true, "over-limit")] {
            let dir = fresh_dir(tag);
            write_log(&dir, &crate::block::__test_chain(20));
            let path = dir.join("chain.log");
            let whole = std::fs::metadata(&path).unwrap().len();
            // A length prefix claiming more than the file holds, then the dangling region (zeros).
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&u32::MAX.to_le_bytes()).unwrap();
            drop(f);
            let dangling = MAX_TORN_TAIL + over as u64;
            OpenOptions::new().write(true).open(&path).unwrap().set_len(whole + dangling).unwrap();
            match ChainLog::open(&dir) {
                Ok(log) if !over => {
                    assert_eq!(log.height(), 20, "at the limit: cut, every whole record kept");
                    assert_eq!(std::fs::metadata(&path).unwrap().len(), whole);
                    assert!(dir.join(format!("chain.log.torn-{whole}")).exists(), "the cut bytes are kept beside the log");
                }
                Err(e) if over => {
                    let msg = e.to_string();
                    assert!(msg.contains(OPEN_REFUSED_CORRUPT), "refusal names itself: {msg}");
                    assert!(msg.contains(&format!("at offset {whole}")), "and the offset: {msg}");
                    assert_eq!(std::fs::metadata(&path).unwrap().len(), whole + dangling, "nothing cut");
                }
                other => panic!("{tag}: unexpected {:?}", other.map(|l| l.height())),
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn get_range_by_height_returns_the_real_requested_heights() {
        // 2026-08-21: get_range_by_height is the height-validated replacement
        // for get_range (see both methods' doc comments — get_range shares
        // get()'s offsets-array fragility, confirmed live to silently return
        // zero blocks for a real range once a node's offsets drift from real
        // height). Note __test_chain builds heights [1..=n], NOT [0..=n-1],
        // so array-index-based get_range is off by one against real height
        // on this fixture even with zero anomaly — exactly the class of
        // assumption get_range_by_height doesn't make. This test checks
        // get_range_by_height directly against real heights, not against
        // get_range (which has no reliable "correct" answer to compare to
        // here).
        let dir = std::env::temp_dir().join(format!("sigil-chainlog-grbh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks = crate::block::__test_chain(200); // real heights 1..=200
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for b in &blocks {
                log.append(b).unwrap();
            }
        }
        // reopen so the method reads through a freshly-rebuilt offsets index.
        let log = ChainLog::open(&dir).unwrap();

        let b = log.get_range_by_height(50, 149);
        assert_eq!(b.len(), 100, "get_range_by_height must find the full clean range");
        let b_heights: Vec<u64> = b.iter().map(|blk| blk.header.height).collect();
        assert_eq!(b_heights, (50u64..=149).collect::<Vec<_>>(), "must return the REAL requested heights");

        // A range at the very tail, and one that overruns the log — both
        // must stop cleanly at the real end rather than erroring.
        let tail = log.get_range_by_height(195, 999);
        assert_eq!(tail.len(), 6, "must stop at the real end of the log (heights 195..=200)");
        assert_eq!(tail.last().unwrap().header.height, 200);

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
    use super::{
        decode_record, encode_record, probe_height, probe_height_fast, ChainLog, PROBE_WINDOW,
        CHAINLOG_DICT_V2, REC_MAGIC, REC_VERSION_V1, REC_VERSION_V2, REC_ZSTD_LEVEL,
    };
    use crate::block::Block;

    /// A v1 record must survive a full encode→decode round trip unchanged. If this ever
    /// fails, every block written since the switch is unreadable — the failure mode the
    /// whole durability change exists to avoid.
    #[test]
    fn v1_record_round_trips_byte_identically() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let rec = encode_v1_for_test(&b);
        assert_eq!(rec[0], REC_MAGIC, "v1 records must carry the magic byte");
        assert_eq!(rec[1], REC_VERSION_V1);
        let back = decode_record(&rec).expect("decode");
        assert_eq!(back.hash(), b.hash(), "round trip must preserve the block hash");
        assert_eq!(back.header.height, b.header.height);
    }

    /// The v1 shape, exactly as every record written 2026-08-27..2026-09-13 was — kept as
    /// a test-side encoder so the reader's v1 branch stays covered forever.
    pub(super) fn encode_v1_for_test(b: &Block) -> Vec<u8> {
        let packed = rmp_serde::to_vec_named(b).unwrap();
        let body = zstd::encode_all(&packed[..], REC_ZSTD_LEVEL).unwrap();
        let mut out = vec![REC_MAGIC, REC_VERSION_V1];
        out.extend_from_slice(&b.header.height.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// v2 (dictionary) is the default writer; it must round-trip, be identifiable, probe
    /// its height without decompressing, and be materially smaller than v1 on a real-shaped
    /// block. A dictionary that stopped matching (someone regenerated the file) would show
    /// up here as the size assertion failing, before it ever reached a node.
    #[test]
    fn v2_dictionary_record_round_trips_probes_and_is_smaller() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let v2 = encode_record(&b).expect("encode");
        assert_eq!(v2[0], REC_MAGIC);
        assert_eq!(v2[1], REC_VERSION_V2, "the default writer is v2");
        assert_eq!(probe_height(&v2), Some(b.header.height), "height probes from the fixed header");
        let back = decode_record(&v2).expect("v2 decodes");
        assert_eq!(back.hash(), b.hash());
        let v1 = encode_v1_for_test(&b);
        assert!(v2.len() < v1.len(), "v2 {} B must beat v1 {} B even on genesis", v2.len(), v1.len());
        // the dictionary is pinned: a changed file is a changed format
        assert_eq!(CHAINLOG_DICT_V2.len(), 65536);
        assert_eq!(&CHAINLOG_DICT_V2[..4], &[0x37, 0xA4, 0x30, 0xEC], "zstd dictionary magic");
    }

    /// `recompress_offline`: a mixed log rewritten in two passes (live, then final with
    /// resume) replays to the identical block sequence, and shrinks.
    #[test]
    fn recompress_two_pass_preserves_the_sequence_and_shrinks() {
        let dir = std::env::temp_dir().join(format!("sigil-chainlog-recompress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks = crate::block::__test_chain(40);
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for (i, b) in blocks.iter().take(30).enumerate() {
                let rec = if i % 2 == 0 { serde_json::to_vec(b).unwrap() } else { encode_v1_for_test(b) };
                log.append_bytes(&rec).unwrap();
            }
        }
        let before = std::fs::metadata(dir.join("chain.log")).unwrap().len();
        // pass 1: live (node "still appending")
        let (n1, _, _) = super::recompress_offline(&dir, true).unwrap();
        assert_eq!(n1, 30);
        assert!(dir.join("chain.log.recompress.state").exists());
        // the node appends 10 more, then stops
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for b in blocks.iter().skip(30) { log.append_bytes(&encode_v1_for_test(b)).unwrap(); }
        }
        // make the mtime guard happy without sleeping 10 s: backdate the file
        let f = std::fs::File::open(dir.join("chain.log")).unwrap();
        let _ = f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60));
        let (n2, b2, a2) = super::recompress_offline(&dir, false).unwrap();
        assert_eq!(n2, 40, "final pass resumes and converts only the tail");
        assert!(a2 < b2, "recompressed {a2} B must be smaller than {b2} B");
        assert!(dir.join("chain.log.pre-v2").exists());
        assert!(!dir.join("chain.log.recompress.state").exists());
        assert!(!dir.join("chain.off").exists(), "the swap drops the old file's dense offsets");
        let after = std::fs::metadata(dir.join("chain.log")).unwrap().len();
        assert!(after < before);
        let mut seen = Vec::new();
        let n = ChainLog::replay(&dir, |b| seen.push(b.hash())).unwrap();
        assert_eq!(n, 40);
        assert_eq!(seen, blocks.iter().map(|b| b.hash()).collect::<Vec<_>>());
        // every record is v2 now, and the fresh index seeks
        let log = ChainLog::open(&dir).unwrap();
        assert_eq!(log.open_stats(), (0, 40), "the first open after the swap scans, and rebuilds chain.off");
        assert_eq!(log.offsets, super::tests::offsets_by_full_scan(&dir));
        assert_eq!(log.get_by_height(blocks[39].header.height).map(|b| b.hash()), Some(blocks[39].hash()));
        drop(log);
        assert_eq!(ChainLog::open(&dir).unwrap().open_stats(), (40, 0), "and the next one reads it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1, v2 and legacy JSON records coexist in one log and replay in order.
    #[test]
    fn mixed_v1_v2_json_records_replay_in_order() {
        let dir = std::env::temp_dir().join(format!("sigil-chainlog-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks = crate::block::__test_chain(6);
        {
            let mut log = ChainLog::open(&dir).unwrap();
            for (i, b) in blocks.iter().enumerate() {
                let rec = match i % 3 {
                    0 => serde_json::to_vec(b).unwrap(),
                    1 => encode_v1_for_test(b),
                    _ => encode_record(b).unwrap(),
                };
                log.append_bytes(&rec).unwrap();
            }
        }
        let mut seen = Vec::new();
        let n = ChainLog::replay(&dir, |b| seen.push(b.header.height)).unwrap();
        assert_eq!(n, 6);
        assert_eq!(seen, blocks.iter().map(|b| b.header.height).collect::<Vec<_>>());
        let log = ChainLog::open(&dir).unwrap();
        for b in &blocks {
            assert_eq!(log.get_by_height(b.header.height).map(|x| x.hash()), Some(b.hash()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Legacy JSON records must keep decoding forever.** The log is append-only and
    /// mixed: every block written before 2026-08-27 is JSON and can never be rewritten.
    /// A reader that lost the ability to read them would strand the entire archive.
    #[test]
    fn legacy_json_records_still_decode_and_mix_with_v1_in_one_log() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let legacy = serde_json::to_vec(&b).expect("json");
        assert_eq!(legacy[0], b'{', "legacy records are identified by a leading brace");
        let from_legacy = decode_record(&legacy).expect("legacy must still decode");
        assert_eq!(from_legacy.hash(), b.hash());

        // and the two forms must agree with each other, not merely each with themselves
        let v1 = encode_record(&b).expect("encode");
        assert_eq!(
            decode_record(&v1).unwrap().hash(),
            from_legacy.hash(),
            "the same block must decode identically from either format"
        );
    }

    /// The height probe must be exact for v1 (read straight out of the record header) and
    /// still work on legacy JSON. Tail-replay uses it to decide which blocks to SKIP, so a
    /// wrong answer silently skips a block that should have been applied.
    #[test]
    fn probe_height_is_exact_for_both_record_formats() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let h = b.header.height;
        assert_eq!(probe_height(&encode_record(&b).unwrap()), Some(h), "v1 probe");
        assert_eq!(probe_height(&serde_json::to_vec(&b).unwrap()), Some(h), "legacy probe");
    }

    /// Garbage and truncated records must return `None`, never panic and never a wrong
    /// block — the tail of an append-only log is a partial write after any crash.
    #[test]
    fn malformed_records_are_refused_not_guessed() {
        assert!(decode_record(&[]).is_none(), "empty");
        assert!(decode_record(&[0xFF, 0x00]).is_none(), "unknown format tag");
        assert!(decode_record(&[REC_MAGIC, 99, 0, 0, 0, 0, 0, 0, 0, 0]).is_none(), "bad version");
        assert!(decode_record(&[REC_MAGIC, REC_VERSION_V1, 1, 2]).is_none(), "truncated header");
        let mut torn = encode_record(&crate::genesis::build_genesis().unwrap()).unwrap();
        torn.truncate(torn.len() / 2);
        assert!(decode_record(&torn).is_none(), "torn compressed body");
    }

    /// The point of the exercise: v1 must actually be materially smaller than JSON on a
    /// real block. Measured 4.40x across 4,000 live blocks; assert a conservative 2x so
    /// the test tracks the property, not the exact ratio.
    #[test]
    fn v1_is_substantially_smaller_than_json() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let j = serde_json::to_vec(&b).unwrap().len();
        let v = encode_record(&b).unwrap().len();
        assert!(v * 2 <= j, "v1 {v} B vs JSON {j} B — expected at least 2x smaller");
    }

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

// ── OFFLINE RECOMPRESSION (2026-09-13) ──────────────────────────────────────────────────
//
// `sigil-node chainlog-recompress <dir>`: rewrite every record of `<dir>/chain.log` in the
// current default format (v2, dictionary) into `chain.log.recompress`, with a fresh sparse
// index beside it, then swap the pair into place and keep the old log as
// `chain.log.pre-v2` until the operator deletes it. Streaming — RAM is one record at a
// time. MEASURED on the live g2 log: v1 1,105 B/blk → v2 ~340 B/blk, so a 13 GB log
// becomes ~4 GB.
//
// OFFLINE ONLY. The running node holds an append handle and an in-RAM offset table for
// the OLD file; swapping under it would corrupt the next append. The caller stops the
// node first (the tool refuses if `chain.log` was modified in the last 10 s, which is the
// cheapest honest liveness test available without a lock file).
/// See the module note above. Returns (records, bytes_before, bytes_after).
///
/// Two-pass use for a node that must not stop for long (the producer): run once with
/// `live = true` while the node runs — it converts everything written so far and leaves
/// `chain.log.recompress` + a `.state` sidecar (source offset, records, output offset) —
/// then stop the node and run again with `live = false`: it resumes from the sidecar,
/// converts only the tail written since, and swaps. The stop lasts seconds, not minutes.
pub fn recompress_offline(dir: &Path, live: bool) -> std::io::Result<(u64, u64, u64)> {
    let src = dir.join("chain.log");
    let meta = std::fs::metadata(&src)?;
    if !live {
        if let Ok(m) = meta.modified() {
            if m.elapsed().map(|d| d.as_secs() < 10).unwrap_or(false) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "chain.log was written in the last 10 s — is the node still running? stop it first (or pass --live for the first pass)",
                ));
            }
        }
    }
    let dst = dir.join("chain.log.recompress");
    let dst_idx = dir.join("chain.idx.recompress");
    let state = dir.join("chain.log.recompress.state");
    // Resume: sidecar = "src_off n out_off before". Both output files are truncated to the
    // recorded lengths first, so a pass killed mid-record can never leave a torn tail behind.
    let (mut src_off, mut n, mut pos_out, mut before): (u64, u64, u64, u64) = (0, 0, 0, 0);
    if let Ok(txt) = std::fs::read_to_string(&state) {
        let v: Vec<u64> = txt.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() == 4 && dst.exists() {
            src_off = v[0]; n = v[1]; pos_out = v[2]; before = v[3];
            OpenOptions::new().write(true).open(&dst)?.set_len(pos_out)?;
            let idx_len = IDX_HEADER_LEN as u64 + n.div_ceil(IDX_EVERY) * IDX_ENTRY_LEN as u64;
            if let Ok(f) = OpenOptions::new().write(true).open(&dst_idx) { let _ = f.set_len(idx_len); }
            eprintln!("  resuming: {n} records already converted ({:.2} GB → {:.2} GB)", before as f64 / 1e9, pos_out as f64 / 1e9);
        }
    }
    let fresh = src_off == 0;
    let mut r = BufReader::with_capacity(8 << 20, File::open(&src)?);
    r.seek(SeekFrom::Start(src_off))?;
    let mut w = BufWriter::with_capacity(8 << 20, if fresh { File::create(&dst)? } else { OpenOptions::new().append(true).open(&dst)? });
    let mut iw = BufWriter::new(if fresh { File::create(&dst_idx)? } else { OpenOptions::new().append(true).open(&dst_idx)? });
    if fresh {
        let mut hdr = [0u8; IDX_HEADER_LEN];
        hdr[..7].copy_from_slice(&IDX_MAGIC);
        hdr[7] = IDX_VERSION;
        iw.write_all(&hdr)?;
    }
    let mut after = pos_out;
    let t0 = std::time::Instant::now();
    let n0 = n;
    // Only convert what existed when this pass started: a live producer keeps appending.
    let src_end = meta.len();
    loop {
        if src_off >= src_end { break; }
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() { break; }
        let len = u32::from_le_bytes(lb) as usize;
        let mut rec = vec![0u8; len];
        if r.read_exact(&mut rec).is_err() { break; } // torn tail: stop, like open() does
        before += 4 + len as u64;
        // Already v2 → copy verbatim (idempotent re-runs); anything else → decode + re-encode.
        let out: Vec<u8> = if rec.len() >= REC_V1_HEADER && rec[0] == REC_MAGIC && rec[1] == REC_VERSION_V2 {
            rec
        } else {
            match decode_record(&rec) {
                Some(b) => encode_record(&b)?,
                None => {
                    eprintln!("⚠ chainlog-recompress: record #{n} ({len} B) does not decode — copied verbatim");
                    rec
                }
            }
        };
        if n % IDX_EVERY == 0 {
            if let Some(h) = probe_height(&out) {
                let mut e = [0u8; IDX_ENTRY_LEN];
                e[..8].copy_from_slice(&h.to_le_bytes());
                e[8..].copy_from_slice(&pos_out.to_le_bytes());
                iw.write_all(&e)?;
            }
        }
        w.write_all(&(out.len() as u32).to_le_bytes())?;
        w.write_all(&out)?;
        pos_out += 4 + out.len() as u64;
        src_off += 4 + len as u64;
        after = pos_out;
        n += 1;
        if n % 500_000 == 0 {
            eprintln!("  … {n} records, {:.1} → {:.1} GB, {:.0} rec/s",
                before as f64 / 1e9, after as f64 / 1e9, (n - n0) as f64 / t0.elapsed().as_secs_f64().max(1e-9));
            w.flush()?; iw.flush()?;
            std::fs::write(&state, format!("{src_off} {n} {pos_out} {before}"))?;
        }
    }
    w.flush()?; w.get_ref().sync_all()?;
    iw.flush()?; iw.get_ref().sync_all()?;
    drop(w); drop(iw);
    std::fs::write(&state, format!("{src_off} {n} {pos_out} {before}"))?;
    if live {
        eprintln!("  live pass done at source offset {src_off} ({n} records); stop the node and run again (without --live) to convert the tail and swap");
        return Ok((n, before, after));
    }
    // Swap: old log → chain.log.pre-v2 (kept), new → chain.log; same for the index.
    let keep = dir.join("chain.log.pre-v2");
    std::fs::rename(&src, &keep)?;
    std::fs::rename(&dst, &src)?;
    let _ = std::fs::rename(dir.join("chain.idx"), dir.join("chain.idx.pre-v2"));
    std::fs::rename(&dst_idx, dir.join("chain.idx"))?;
    // Every record moved: the dense offsets describe the old file. The next open rebuilds them.
    let _ = std::fs::remove_file(dir.join("chain.off"));
    let _ = std::fs::remove_file(&state);
    Ok((n, before, after))
}
