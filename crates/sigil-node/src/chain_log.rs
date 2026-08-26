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

/// ── RECORD CODEC ────────────────────────────────────────────────────────────────
///
/// A record's framing is unchanged — `[u32 LE len][payload]`. Only the PAYLOAD encoding
/// is versioned, and both forms coexist in one log forever:
///
/// * **legacy**: the payload IS `serde_json` and therefore starts with `{` (0x7B).
/// * **v1**: `[MAGIC][VERSION][height: u64 LE][zstd(MessagePack(Block))]`.
///
/// # Why this changed
///
/// JSON measured **3,940 bytes per block** on blocks carrying essentially zero
/// transactions, because every 32-byte hash is written as a decimal array
/// (`[153,13,136,…]` — up to 4 characters per byte). At 26 blk/s that is **3.23 TB/year**,
/// ~44× Bitcoin's bytes/day for a chain recording nothing. Nobody volunteers to keep an
/// archival copy of that, and an archive nobody keeps is the thing that makes a ledger
/// die. Size here is a decentralisation property, not a micro-optimisation.
///
/// # Why MessagePack and NOT bincode
///
/// Measured on 4,000 real blocks (`examples/chainlog_codec_bench.rs`):
///
/// | codec | bytes/block | vs JSON | self-describing |
/// |---|---|---|---|
/// | JSON (was) | 3,940 | 1.00× | yes |
/// | MessagePack + zstd | **896** | **4.40×** | **yes** |
/// | bincode + zstd | 428 | 9.21× | **NO** |
///
/// bincode is half the size again and was still rejected, deliberately. bincode is not
/// self-describing: it stores field ORDER and nothing else, so adding one field to `Block`
/// or `SigilBlockHeaderV0` makes every previously-written record undecodable. This chain
/// already depends on the opposite property — `sigil-header` carries several
/// `#[serde(default)]` fields whose comments say exactly that, e.g. *"keeps pre-tx-count
/// blocks decoding to 0"*. Historical blocks are ALREADY being read back through a struct
/// that has grown since they were written. Choosing bincode would trade 2× on disk for the
/// guarantee that the next header field silently bricks the archive — the precise opposite
/// of the durability this change exists to buy.
///
/// MessagePack keeps field names, so an old record still round-trips through a newer
/// struct exactly as JSON does today, and `zstd` recovers most of what the names cost.
const REC_MAGIC: u8 = 0xB5;
const REC_VERSION_V1: u8 = 1;
/// `MAGIC | VERSION | height(8)` — the fixed part before the compressed body.
const REC_V1_HEADER: usize = 10;
/// zstd level 3: measured 896 B/blk. Higher levels gain little on records this small and
/// cost append latency on the settlement path, which is the one place that must stay hot.
const REC_ZSTD_LEVEL: i32 = 3;

/// Encode a block into a record payload. **The single encoder** — every writer goes through
/// here so a format change can never be applied to only some call sites.
pub fn encode_record(block: &Block) -> std::io::Result<Vec<u8>> {
    // Escape hatch: keeps the old format writable for bisecting a suspected codec bug
    // against a known-good reader. Reading never needs it — both forms always decode.
    if std::env::var("SIGIL_CHAINLOG_JSON").as_deref() == Ok("1") {
        return serde_json::to_vec(block)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
    }
    let packed = rmp_serde::to_vec_named(block)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body = zstd::encode_all(&packed[..], REC_ZSTD_LEVEL)?;
    let mut out = Vec::with_capacity(REC_V1_HEADER + body.len());
    out.push(REC_MAGIC);
    out.push(REC_VERSION_V1);
    out.extend_from_slice(&block.header.height.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a record payload written in EITHER format. **The single decoder.**
///
/// Returns `None` rather than panicking on a torn or unknown record: every caller already
/// treats a failed decode as "stop the scan here", which is the correct behaviour for an
/// append-only log whose tail may be a partial write after a crash.
pub fn decode_record(payload: &[u8]) -> Option<Block> {
    match payload.first()? {
        // Legacy JSON — every record written before 2026-08-27.
        b'{' => serde_json::from_slice(payload).ok(),
        &REC_MAGIC => {
            if payload.len() < REC_V1_HEADER || payload[1] != REC_VERSION_V1 {
                return None;
            }
            let raw = zstd::decode_all(&payload[REC_V1_HEADER..]).ok()?;
            rmp_serde::from_slice(&raw).ok()
        }
        _ => None,
    }
}

/// Extract `header.height` from a record payload without decoding the whole block.
///
/// For v1 this is exact and O(1) — the height is stored in the record header precisely so
/// the tail-replay skip path never has to decompress a block it is going to discard. For
/// legacy JSON it falls back to the byte-scan heuristic, which returns `None` on any doubt.
fn probe_height(payload: &[u8]) -> Option<u64> {
    match payload.first()? {
        &REC_MAGIC if payload.len() >= REC_V1_HEADER && payload[1] == REC_VERSION_V1 => {
            Some(u64::from_le_bytes(payload[2..10].try_into().ok()?))
        }
        b'{' => probe_height_fast(payload),
        _ => None,
    }
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
                // seek_relative (not seek(SeekFrom::Start)) — a BufReader's Seek impl
                // unconditionally discards its internal buffer on every call, turning
                // this scan into one raw unbuffered syscall PER RECORD (each pulling a
                // fresh OS page just to read the next 4-byte length prefix). At tens of
                // millions of records that's tens of millions of syscalls and hundreds
                // of GB of redundant reads — the actual cause of a multi-hour open() on
                // a large chain.log. seek_relative stays inside the buffer whenever the
                // skip distance fits (always true here: records are ~hundreds of bytes,
                // the buffer is 8 KB), so the common case costs no syscall at all.
                if r.seek_relative(rec as i64).is_err() {
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
        self.offsets.push(self.bytes_len);
        self.bytes_len += 4 + bytes.len() as u64;
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
        let dir = self.path.parent()?;
        let start = Self::idx_seek_offset(dir, height, &self.path).unwrap_or(0);
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
        let Some(dir) = self.path.parent() else { return out };
        let start = Self::idx_seek_offset(dir, from, &self.path).unwrap_or(0);
        let Ok(f) = File::open(&self.path) else { return out };
        let mut r = BufReader::new(f);
        if r.seek(SeekFrom::Start(start)).is_err() { return out; }
        loop {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() { break; } // EOF
            let n = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() { break; } // torn tail record
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
        decode_record, encode_record, probe_height, probe_height_fast, PROBE_WINDOW, REC_MAGIC,
        REC_VERSION_V1,
    };

    /// A v1 record must survive a full encode→decode round trip unchanged. If this ever
    /// fails, every block written since the switch is unreadable — the failure mode the
    /// whole durability change exists to avoid.
    #[test]
    fn v1_record_round_trips_byte_identically() {
        let b = crate::genesis::build_genesis().expect("genesis");
        let rec = encode_record(&b).expect("encode");
        assert_eq!(rec[0], REC_MAGIC, "v1 records must carry the magic byte");
        assert_eq!(rec[1], REC_VERSION_V1);
        let back = decode_record(&rec).expect("decode");
        assert_eq!(back.hash(), b.hash(), "round trip must preserve the block hash");
        assert_eq!(back.header.height, b.header.height);
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
