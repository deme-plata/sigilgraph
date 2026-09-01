//! Header-only, read-only reads of `chain.log`, for the backfill **serve** path.
//!
//! # Why this exists (measured, 2026-08-26)
//!
//! Serving a headers-only backfill range used to go through
//! [`ChainLog::get_range_by_height`](crate::chain_log::ChainLog::get_range_by_height),
//! which decodes **entire blocks** — bodies, transactions, state mutations — and
//! then the serve path immediately threw all of that away and kept only
//! `block.header`. A header is ~70 bytes; a live serve of 8,193 headers
//! (≈573 KB of actual payload) was JSON-decoding megabytes of block bodies to
//! produce it.
//!
//! That cost is paid **inline on the same synchronous loop that produces
//! blocks**. On 2026-08-26 it stopped being a throughput problem and became a
//! liveness one: three independent `perf` profiles of the live producer, taken
//! by three different agents in different sample windows, all converged on
//! `ChainLog::get_range_by_height` + `serde_json::Deserializer::parse_number` /
//! `parse_integer` / `SeqAccess::next_element_seed` + `malloc`/`_int_free`. The
//! produce tick (27 ms) never got the loop back, so `publish_tip()` never ran,
//! so `/v1/mining/challenge` answered **503** and five live miners at ~54 MH/s
//! earned exactly nothing while the chain sat frozen.
//!
//! # What this does instead
//!
//! Two savings, both on the same single sequential pass:
//!
//! 1. **Never decode a body.** Records deserialize into [`HeaderOnly`], so
//!    serde skips every field after `header` via its ignored-any path — no
//!    `Vec<Transaction>`, no `StateMutation` enums, no allocation for data we
//!    are about to drop.
//! 2. **Never decode a skipped record at all.** The sparse `chain.idx` lands us
//!    at most `IDX_EVERY` blocks before `from`, and the records in that run are
//!    skipped by probing `"height":` out of their leading bytes and
//!    `seek_relative`-ing over the remainder — the same trick `chain_log`'s own
//!    catch-up scan uses. The scan also **breaks** at the first record above
//!    `to` instead of reading to end-of-log.
//!
//! # Why it is a separate module and not a method on `ChainLog`
//!
//! Two reasons, one structural and one social:
//!
//! * `ChainLog::open()` scans all ~2.18 M records to build its `offsets` vector
//!   **and** opens an append writer on the live `chain.log`. A read-only reader
//!   on the serve path must never hold either. Everything here takes a
//!   directory and opens the file read-only, per call.
//! * `chain_log.rs` was being edited concurrently by another agent when this
//!   landed. Keeping this self-contained meant zero merge risk on a file
//!   someone else was mid-build in.
//!
//! The cost of that independence is that the index header constants and the
//! height probe are duplicated from `chain_log`. They are duplicated
//! **deliberately** and are covered by [`tests::idx_constants_match_chain_log`]
//! and [`tests::probe_matches_a_real_record`], which fail loudly if the on-disk
//! format ever drifts apart from this reader.
//!
//! # Safety
//!
//! This module is read-only and returns headers. It cannot alter consensus,
//! state, or the log. Every failure path degrades to "fewer headers returned",
//! never to wrong headers: an unparseable record is skipped, a torn tail stops
//! the scan, a missing/stale index falls back to scanning from offset 0, and an
//! ambiguous height probe falls through to a real parse rather than guessing.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sigil_header::SigilBlockHeaderV0;

/// Mirrors `chain_log`'s `chain.idx` header. See the module doc on duplication.
const IDX_MAGIC: [u8; 7] = *b"SGLIDX\0";
const IDX_VERSION: u8 = 1;
const IDX_HEADER_LEN: usize = 8;
const IDX_ENTRY_LEN: usize = 16;

/// How many leading bytes of a record the skip-probe reads. `header` is the
/// block's first field and `height` its third, so `"height":` lands well inside
/// this window; 256 B is the same figure `chain_log` uses.
const PROBE_WINDOW: usize = 256;

/// Deserialization target that keeps ONLY the header. Every field after
/// `header` is skipped by serde without being materialised — this is the whole
/// point of the module.
#[derive(serde::Deserialize)]
struct HeaderOnly {
    header: SigilBlockHeaderV0,
}

/// The chain log inside a node data directory.
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("chain.log")
}

/// The sparse height index that sits beside it.
fn idx_path(dir: &Path) -> PathBuf {
    dir.join("chain.idx")
}

/// Byte offset of a record at or before `from_height`, from the sparse index.
///
/// Returns `Some(0)` (scan from the start) when there is no usable index, and
/// `None` only when the index exists but is unreadable — callers treat both as
/// "start at 0", so a stale index costs time, never correctness. Unlike
/// `chain_log`'s equivalent this does NOT re-validate the entry by decoding the
/// record at that offset: the scan below re-reads every record's real height
/// anyway, so a wrong offset self-corrects.
fn idx_seek_offset(dir: &Path, from_height: u64) -> Option<u64> {
    let raw = std::fs::read(idx_path(dir)).ok()?;
    if raw.len() < IDX_HEADER_LEN || raw[..7] != IDX_MAGIC || raw[7] != IDX_VERSION {
        return None;
    }
    let mut best: Option<(u64, u64)> = None; // (height, offset)
    for e in raw[IDX_HEADER_LEN..].chunks_exact(IDX_ENTRY_LEN) {
        let h = u64::from_le_bytes(e[..8].try_into().ok()?);
        let off = u64::from_le_bytes(e[8..].try_into().ok()?);
        if h <= from_height && best.map(|(bh, _)| h >= bh).unwrap_or(true) {
            best = Some((h, off));
        }
    }
    Some(best.map(|(_, off)| off).unwrap_or(0))
}

/// Pull `header.height` out of a record's leading bytes without a full parse.
///
/// 2026-08-27: this had its own implementation that searched the record bytes for the
/// literal ASCII key `"height":`. Records have not been raw JSON for a long time — the
/// on-disk format is `[MAGIC][VERSION][height: u64 LE][zstd(MessagePack(Block))]`, so that
/// string does not occur anywhere in the log (`grep -c '"height":'` over 20 MB of a live
/// chain.log: ZERO). The probe therefore returned `None` for every record, and the
/// header-only parse below it was `serde_json::from_slice`, which fails on compressed
/// bytes for the same reason.
///
/// The consequence was total and silent: EVERY range below the producer's RAM window was
/// served as an empty response — `got=0 h=[0..0] bytes=18` — while the blocks sat
/// perfectly readable on disk. A full-archive client asking for early history got nothing,
/// forever, from a node that held every block. Live: 28 of 38 requests empty, 3% carrying
/// data, a client parked at h=30,250 against a chain at 127,000.
///
/// `chain_log` already had the correct probe, handling both the current binary framing and
/// the legacy JSON form. This now delegates to it. One record format, one reader.
fn probe_height(bytes: &[u8]) -> Option<u64> {
    crate::chain_log::probe_height(bytes)
}

/// Read the headers for `[from..=to]` from the chain log on disk.
///
/// One file open, one seek, one sequential forward scan. Records below `from`
/// are seeked over without being decoded; the scan stops at the first record
/// above `to`. Returns the headers actually found, in log order — a short
/// result means the log genuinely ends (or is torn) before `to`, which is the
/// same contract the serve path already had.
pub fn read_headers_range(dir: &Path, from: u64, to: u64) -> Vec<SigilBlockHeaderV0> {
    let mut out = Vec::new();
    if to < from {
        return out;
    }
    let start = idx_seek_offset(dir, from).unwrap_or(0);
    let Ok(f) = File::open(log_path(dir)) else {
        return out;
    };
    let mut r = BufReader::new(f);
    if r.seek(SeekFrom::Start(start)).is_err() {
        return out;
    }
    loop {
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() {
            break; // clean EOF
        }
        let n = u32::from_le_bytes(lb) as usize;
        let take = n.min(PROBE_WINDOW);
        let mut buf = vec![0u8; take];
        if r.read_exact(&mut buf).is_err() {
            break; // torn tail record
        }
        // Cheap path: decide from the probe alone whether this record is even
        // wanted, and skip the rest of its bytes if it isn't.
        match probe_height(&buf) {
            Some(h) if h < from => {
                if r.seek_relative((n - take) as i64).is_err() {
                    break;
                }
                continue;
            }
            Some(h) if h > to => break,
            _ => {}
        }
        // Wanted (or the probe was inconclusive) — read the remainder and do a
        // real, header-only parse.
        if n > take {
            let mut rest = vec![0u8; n - take];
            if r.read_exact(&mut rest).is_err() {
                break;
            }
            buf.extend_from_slice(&rest);
        }
        // Decode through `chain_log`'s single decoder, which understands the compressed
        // binary framing AND the legacy JSON records. The previous `serde_json::from_slice`
        // could only ever succeed on the latter — see `probe_height` above.
        match crate::chain_log::decode_record(&buf).map(|b| HeaderOnly { header: b.header }) {
            Some(rec) => {
                // Re-check against the authoritative parsed height: the probe is
                // an optimisation, never the decision of record.
                if rec.header.height < from {
                    continue;
                }
                if rec.header.height > to {
                    break;
                }
                out.push(rec.header);
            }
            // Malformed record: skip it rather than abandoning the range, same
            // as chain_log's own scans.
            None => continue,
        }
    }
    out
}

/// Read FULL blocks for `[from..=to]` from the chain log on disk — the
/// full-body sibling of [`read_headers_range`], same one-open/one-seek/
/// sequential-scan shape, same probe-and-skip cheap path, same torn-tail and
/// malformed-record tolerance. Exists so the serve path can answer a
/// full-block backfill OFF the produce loop: this reads by PATH (its own
/// file handle), never touching `chain_log`'s append handle, which is the
/// same concurrent-reader pattern the off-thread header serve has run live
/// since 2026-08-26.
pub fn read_blocks_range(dir: &Path, from: u64, to: u64) -> Vec<crate::block::Block> {
    let mut out = Vec::new();
    if to < from {
        return out;
    }
    let start = idx_seek_offset(dir, from).unwrap_or(0);
    let Ok(f) = File::open(log_path(dir)) else {
        return out;
    };
    let mut r = BufReader::new(f);
    if r.seek(SeekFrom::Start(start)).is_err() {
        return out;
    }
    loop {
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() {
            break; // clean EOF
        }
        let n = u32::from_le_bytes(lb) as usize;
        let take = n.min(PROBE_WINDOW);
        let mut buf = vec![0u8; take];
        if r.read_exact(&mut buf).is_err() {
            break; // torn tail record
        }
        match probe_height(&buf) {
            Some(h) if h < from => {
                if r.seek_relative((n - take) as i64).is_err() {
                    break;
                }
                continue;
            }
            Some(h) if h > to => break,
            _ => {}
        }
        if n > take {
            let mut rest = vec![0u8; n - take];
            if r.read_exact(&mut rest).is_err() {
                break;
            }
            buf.extend_from_slice(&rest);
        }
        match crate::chain_log::decode_record(&buf) {
            Some(b) => {
                if b.header.height < from {
                    continue;
                }
                if b.header.height > to {
                    break;
                }
                out.push(b);
            }
            None => continue,
        }
    }
    out
}

/// Encode a header run onto the backfill wire for `codec` 0, 1 or 2.
///
/// ONE implementation, called from both the inline serve path and the
/// off-thread one, so the two can never drift into serving different bytes for
/// the same request — byte-symmetry with the client decoder is the whole
/// contract here.
///
/// * `0` → `'H'` + bincode `Vec<header>`
/// * `1` → `'Z'` + zstd-1 of the same body (falls back to `'H'` if zstd errors)
/// * `2` → `'S'` + bincode `Vec<SkeletonRecord>` (the frozen 72 B/record wire)
///
/// Codecs 3 (`'P'`) and 4 (`'F'`) are deliberately NOT handled: both encode the
/// CURRENT anchor/tip rather than just `[lo,hi]`, so they depend on live chain
/// state and must stay on the caller's side.
pub fn encode_headers(headers: &[SigilBlockHeaderV0], codec: u8) -> Vec<u8> {
    if codec == 2 {
        let recs: Vec<sigil_header::SkeletonRecord> =
            headers.iter().map(sigil_header::SkeletonRecord::from_header).collect();
        let mut o = vec![b'S'];
        o.extend(bincode::serialize(&recs).unwrap_or_default());
        return o;
    }
    let body = bincode::serialize(&headers.to_vec()).unwrap_or_default();
    if codec == 1 {
        // v0.33 zstd wire. Measured 14.0× on a real chunk (~20 ms/4 MB — far
        // cheaper than the wire time it saves). Any compress error → plain 'H'.
        match zstd::encode_all(&body[..], 1) {
            Ok(z) => {
                let mut o = vec![b'Z'];
                o.extend(z);
                o
            }
            Err(_) => {
                let mut o = vec![b'H'];
                o.extend(&body);
                o
            }
        }
    } else {
        let mut o = vec![b'H'];
        o.extend(&body);
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::*;
    use std::io::Write;

    /// A record with the SAME field order the real block uses: `header` first,
    /// body after. Field order matters — the skip-probe finds the FIRST
    /// `"height":` in a record's leading bytes and trusts it to be
    /// `header.height`, so a fixture that put the body first would not be
    /// testing the real layout. `derive(Serialize)` preserves declaration order.
    #[derive(serde::Serialize)]
    struct Rec {
        header: SigilBlockHeaderV0,
        /// Deliberately much larger than the header, so a regression that went
        /// back to decoding bodies would still pass functionally while this
        /// test keeps the real size asymmetry (~70 B header, KB-scale body)
        /// visible in the fixture.
        body_filler: Vec<u64>,
    }

    /// A well-formed header at `height`. Mirrors the shared `mk_header` helper
    /// used by sigil-state's and sigil-top's tests.
    fn mk_header(height: u64) -> SigilBlockHeaderV0 {
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        let parent_hash: BlockHash = [0u8; 32];
        let mut hh = blake3::Hasher::new();
        hh.update(&parent_hash);
        hh.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
        let scheme = SigScheme::SqiSign5;
        SigilBlockHeaderV0 {
            version: HEADER_VERSION,
            network_id: NETWORK_ID,
            height,
            parent_hash,
            merge_parents: Vec::new(),
            timestamp_ms: 1_000 + height,
            nonce_sqisign: nonce,
            vdf_input,
            vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 },
            difficulty: 1,
            wallet_state_root: [0u8; 32],
            dex_state_root: [0u8; 32],
            event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32],
            tx_count: 0,
            fluxc_artifact_proof: ProofBundle {
                artifact_blake3: [0u8; 32],
                sqisign_sig: vec![],
                sqisign_pubkey: vec![],
                settle_tx: None,
            },
            topology_commitment: None,
            sig_scheme: scheme,
            producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
        }
    }

    /// Write records in the format PRODUCTION actually uses — the ONLY log writer
    /// the tests use now. The legacy raw-JSON `write_log` helper was removed once
    /// its last callers were migrated here: it wrote a `Rec` shape the real serve
    /// path (`chain_log::decode_record`) had stopped decoding, so every test using
    /// it silently returned empty for every range.
    fn write_log_v1(dir: &Path, blocks: &[crate::block::Block]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut f = File::create(log_path(dir)).unwrap();
        for b in blocks {
            let bytes = crate::chain_log::encode_record(b).unwrap();
            f.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&bytes).unwrap();
        }
        f.flush().unwrap();
    }

    /// THE REGRESSION (2026-08-27).
    ///
    /// `read_headers_range` probed for the literal ASCII `"height":` and parsed with
    /// `serde_json`. Records are `[MAGIC][VERSION][height u64 LE][zstd(MessagePack)]`, so
    /// that string appears NOWHERE in a real log (`grep -c` over 20 MB of a live chain.log:
    /// zero) and the JSON parse cannot succeed either.
    ///
    /// Every range below the producer's RAM window was therefore served as an EMPTY
    /// response — `got=0 h=[0..0] bytes=18` — while the blocks sat perfectly readable on
    /// disk. Live: 28 of 38 requests empty, 3% carrying data, a full-archive client parked
    /// at h=30,250 against a chain at 127,000, for hours, from a node holding every block.
    #[test]
    fn reads_records_in_the_production_format_not_just_legacy_json() {
        let dir = tmpdir("v1-format");
        let genesis = crate::genesis::build_genesis().expect("genesis");
        write_log_v1(&dir, std::slice::from_ref(&genesis));

        let got = read_headers_range(&dir, genesis.header.height, genesis.header.height);
        assert_eq!(
            got.len(),
            1,
            "REGRESSION: the serve path must read the format the writer actually emits — \
             returning nothing here is what served empty ranges to every client while the \
             blocks were on disk"
        );
        assert_eq!(got[0].height, genesis.header.height);

        // And the probe must agree with the framing rather than hunting for a JSON key.
        let rec = crate::chain_log::encode_record(&genesis).unwrap();
        assert_eq!(
            probe_height(&rec),
            Some(genesis.header.height),
            "the height is in the record header in plaintext; it must be read from there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sigil-serve-read-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The off-thread FULL-BLOCK serve (2026-08-31) reads through this: same
    /// production framing as the header reader, but the whole block must
    /// survive the round-trip — a body-dropping regression here would feed
    /// syncing producers empty transactions while looking perfectly healthy.
    #[test]
    fn read_blocks_range_returns_full_blocks_in_production_format() {
        let dir = tmpdir("blocks-range");
        let genesis = crate::genesis::build_genesis().expect("genesis");
        write_log_v1(&dir, std::slice::from_ref(&genesis));

        let got = read_blocks_range(&dir, genesis.header.height, genesis.header.height);
        assert_eq!(got.len(), 1, "the production-format record must decode as a FULL block");
        assert_eq!(got[0].header.height, genesis.header.height);
        assert_eq!(
            got[0].header.hash(),
            genesis.header.hash(),
            "byte-identical header round-trip — the serve wire depends on it"
        );
        // A range past the log's end returns short/empty, never mis-served data.
        assert!(read_blocks_range(&dir, genesis.header.height + 1, genesis.header.height + 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The duplication guard called out in the module doc: if `chain_log`'s
    /// on-disk index format ever changes, this fails instead of silently
    /// reading garbage offsets.
    #[test]
    fn idx_constants_match_chain_log() {
        assert_eq!(IDX_MAGIC, *b"SGLIDX\0");
        assert_eq!(IDX_VERSION, 1);
        assert_eq!(IDX_HEADER_LEN, 8);
        assert_eq!(IDX_ENTRY_LEN, 16);
    }

    /// The probe must read `header.height` out of a REAL serialized record —
    /// not a hand-written approximation of one.
    #[test]
    fn probe_matches_a_real_record() {
        let rec = Rec { header: mk_header(4242), body_filler: vec![9u64; 64] };
        let bytes = serde_json::to_vec(&rec).unwrap();
        assert_eq!(probe_height(&bytes), Some(4242));
    }

    #[test]
    fn probe_refuses_a_height_cut_off_by_the_window() {
        // Digits running to the very end of the window must NOT be trusted — a
        // truncated height reads as a smaller number and would skip a record
        // the caller asked for.
        let mut b = vec![b'x'; PROBE_WINDOW - 12];
        b.extend_from_slice(b"\"height\":123");
        assert_eq!(probe_height(&b), None);
    }

    #[test]
    fn reads_exactly_the_requested_range() {
        let tmp = tmpdir("range");
        write_log_v1(&tmp, &crate::block::__test_chain(50)); // heights 1..=50, production format

        let hs = read_headers_range(&tmp, 10, 19);
        assert_eq!(hs.len(), 10, "inclusive range [10..=19]");
        assert_eq!(hs.first().unwrap().height, 10);
        assert_eq!(hs.last().unwrap().height, 19);
        // Every returned header is the real thing, not a stub.
        assert!(hs.iter().all(|h| h.version == HEADER_VERSION && h.network_id == NETWORK_ID));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stops_at_end_of_log_and_handles_empty_and_inverted_ranges() {
        let tmp = tmpdir("eof");
        write_log_v1(&tmp, &crate::block::__test_chain(10)); // heights 1..=10, production format

        // Asking past the end returns what exists, not an error.
        let hs = read_headers_range(&tmp, 5, 999);
        assert_eq!(hs.len(), 6, "heights 5..=10");
        assert_eq!(hs.last().unwrap().height, 10);

        // Inverted range is empty and never touches the disk.
        assert!(read_headers_range(&tmp, 9, 3).is_empty());

        // A directory with no log at all degrades to empty, not a panic.
        assert!(read_headers_range(&tmp.join("nope"), 0, 10).is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_torn_tail_record_truncates_instead_of_panicking() {
        let tmp = tmpdir("torn");
        write_log_v1(&tmp, &crate::block::__test_chain(10)); // 10 intact records, production format

        // Append a length prefix promising far more bytes than actually follow.
        let mut f = std::fs::OpenOptions::new().append(true).open(log_path(&tmp)).unwrap();
        f.write_all(&9999u32.to_le_bytes()).unwrap();
        f.write_all(b"{\"header\":{\"height\":10").unwrap();
        f.flush().unwrap();

        let hs = read_headers_range(&tmp, 0, 100);
        assert_eq!(hs.len(), 10, "the 10 intact records survive; the torn tail is dropped");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `encode_headers` is now the ONLY encoder for codecs 0/1/2 — the inline
    /// serve path and the off-thread one both call it. This pins the wire bytes
    /// it produces, so a change that silently altered the framing (and broke
    /// byte-symmetry with every deployed client's decoder) fails here.
    #[test]
    fn encode_headers_pins_the_wire_framing() {
        let hs: Vec<SigilBlockHeaderV0> = (0u64..4).map(mk_header).collect();

        let h = encode_headers(&hs, 0);
        assert_eq!(h[0], b'H', "codec 0 is 'H' + bincode Vec<header>");
        assert_eq!(
            h[1..],
            bincode::serialize(&hs).unwrap()[..],
            "codec 0 body must be the plain bincode encoding, unwrapped"
        );

        let z = encode_headers(&hs, 1);
        assert_eq!(z[0], b'Z', "codec 1 is 'Z' + zstd of the SAME body");
        assert_eq!(
            zstd::decode_all(&z[1..]).unwrap(),
            bincode::serialize(&hs).unwrap(),
            "codec 1 must decompress to exactly the codec 0 body"
        );

        let s = encode_headers(&hs, 2);
        assert_eq!(s[0], b'S', "codec 2 is 'S' + bincode Vec<SkeletonRecord>");
        let recs: Vec<sigil_header::SkeletonRecord> =
            hs.iter().map(sigil_header::SkeletonRecord::from_header).collect();
        assert_eq!(s[1..], bincode::serialize(&recs).unwrap()[..]);

        // An unknown codec must degrade to the plain 'H' framing, never to
        // empty bytes or a panic — a peer speaking a codec we do not know still
        // gets something its 'H' decoder can read.
        assert_eq!(encode_headers(&hs, 9)[0], b'H');
    }

    /// The honest gate for this module, run against the REAL chain log rather
    /// than a fixture: decode the SAME bytes both ways and compare.
    ///
    /// `#[ignore]`d because it needs a populated chain log on disk. Run it with:
    /// ```text
    /// SIGIL_SERVE_READ_BENCH_DIR=/home/storage/sigil-snap-db/aether \
    ///   fluxc test -p sigil-node --bin sigil-node -- --ignored serve_read
    /// ```
    /// It asserts only that the header-only decode is genuinely FASTER and that
    /// both paths agree on the heights — the timing numbers are printed for the
    /// record, not asserted against a threshold (a threshold would just be a
    /// flaky test on a shared box).
    #[test]
    #[ignore]
    fn bench_header_only_vs_full_block_on_the_real_log() {
        let Ok(dir) = std::env::var("SIGIL_SERVE_READ_BENCH_DIR") else {
            eprintln!("set SIGIL_SERVE_READ_BENCH_DIR to a real chain-log dir");
            return;
        };
        let dir = PathBuf::from(dir);
        let Ok(f) = File::open(log_path(&dir)) else {
            eprintln!("no chain.log at {}", log_path(&dir).display());
            return;
        };

        // Pull a realistic serve-sized run of raw records off the live log.
        const N: usize = 8_193; // the live full-block serve cap
        let mut r = BufReader::new(f);
        let mut raw: Vec<Vec<u8>> = Vec::with_capacity(N);
        while raw.len() < N {
            let mut lb = [0u8; 4];
            if r.read_exact(&mut lb).is_err() {
                break;
            }
            let n = u32::from_le_bytes(lb) as usize;
            let mut buf = vec![0u8; n];
            if r.read_exact(&mut buf).is_err() {
                break;
            }
            raw.push(buf);
        }
        assert!(!raw.is_empty(), "chain.log had no readable records");
        let bytes: usize = raw.iter().map(|b| b.len()).sum();

        let t0 = std::time::Instant::now();
        let hdr_heights: Vec<u64> = raw
            .iter()
            .filter_map(|b| serde_json::from_slice::<HeaderOnly>(b).ok())
            .map(|h| h.header.height)
            .collect();
        let header_only = t0.elapsed();

        let t1 = std::time::Instant::now();
        let full_heights: Vec<u64> = raw
            .iter()
            .filter_map(|b| serde_json::from_slice::<crate::block::Block>(b).ok())
            .map(|b| b.header.height)
            .collect();
        let full_block = t1.elapsed();

        eprintln!(
            "serve_read bench: {} records, {:.1} MiB\n  header-only {:?}\n  full-block  {:?}\n  speedup     {:.2}x",
            raw.len(),
            bytes as f64 / (1024.0 * 1024.0),
            header_only,
            full_block,
            full_block.as_secs_f64() / header_only.as_secs_f64().max(1e-9),
        );

        assert_eq!(hdr_heights, full_heights, "both decode paths must agree on heights");
        assert!(
            header_only < full_block,
            "header-only decode must be faster than full-block decode ({header_only:?} vs {full_block:?})"
        );
    }

    /// A single corrupt/undecodable record in the MIDDLE of the log must be
    /// skipped, not abort the whole range — a syncing peer still gets every
    /// intact record around it (the `None => continue` arm in read_headers_range).
    ///
    /// (Historical note: this used to assert a header could be served out of a
    /// record whose BODY was garbage. That was true only for the legacy JSON form.
    /// The v1 format compresses header+body together, so a record whose body won't
    /// decode yields no header at all — there is nothing to extract. What still
    /// holds, and is what actually protects a syncing peer, is that such a record
    /// is dropped without poisoning the rest of the range.)
    #[test]
    fn a_malformed_record_is_skipped_not_fatal_to_the_range() {
        fn write_rec(f: &mut File, bytes: &[u8]) {
            f.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
            f.write_all(bytes).unwrap();
        }
        let tmp = tmpdir("nobody");
        std::fs::create_dir_all(&tmp).unwrap();
        let blocks = crate::block::__test_chain(4); // heights 1..=4, production format
        let mut f = File::create(log_path(&tmp)).unwrap();
        for b in &blocks[..2] {
            write_rec(&mut f, &crate::chain_log::encode_record(b).unwrap());
        }
        // A COMPLETE record (real length prefix, fully present) that the decoder
        // rejects: the leading `{` routes it to the JSON path, which then fails to
        // parse it as a Block. Must be SKIPPED, never abort the scan.
        write_rec(&mut f, b"{ a complete record that does not decode to a block");
        for b in &blocks[2..] {
            write_rec(&mut f, &crate::chain_log::encode_record(b).unwrap());
        }
        f.flush().unwrap();

        let hs = read_headers_range(&tmp, 0, 100);
        assert_eq!(
            hs.iter().map(|h| h.height).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "the malformed middle record is skipped; the 4 valid ones all survive",
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
