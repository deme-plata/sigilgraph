//! sigil-record — the ONE block-record codec, shared by writer and every reader.
//!
//! A record's payload is versioned; every form coexists in one log and on one gossip
//! topic forever:
//!
//! * **legacy**: the payload IS `serde_json` and therefore starts with `{` (0x7B).
//! * **v1**: `[MAGIC 0xB5][1][height: u64 LE][zstd(MessagePack(block))]`.
//! * **v2**: `[MAGIC 0xB5][2][height: u64 LE][zstd-with-dictionary(MessagePack(block))]` —
//!   the same MessagePack body, compressed against the trained `dict/chainlog-v2.zdict`.
//!
//! # Why MessagePack and NOT bincode
//!
//! Measured on 4,000 real blocks (`sigil-node/examples/chainlog_codec_bench.rs`):
//!
//! | codec | bytes/block | vs JSON | self-describing |
//! |---|---|---|---|
//! | JSON (was) | 3,940 | 1.00× | yes |
//! | MessagePack + zstd | **896** | **4.40×** | **yes** |
//! | bincode + zstd | 428 | 9.21× | **NO** |
//!
//! bincode is half the size again and was still rejected, deliberately: it stores field
//! ORDER and nothing else, so adding one field to the block makes every previously written
//! record undecodable, and it cannot decode SIGIL's internally-tagged enums at all
//! (`SigilEvent`/`SigilTx` `tag = "kind"` — the 2026-09-11 follower stall). MessagePack
//! keeps field names, so an old record round-trips through a newer struct exactly as JSON
//! does, and zstd recovers most of what the names cost.
//!
//! # Why this is its own crate
//!
//! Until 2026-09-14 the codec lived inside the `sigil-node` binary. `sigil-top` (the light
//! client, cross-built for Windows without C) therefore had no way to read a live gossip
//! record: it subscribed to a dead g0 topic and parsed JSON, so its "live tip from gossip"
//! path had been silently inert since the g2 wire went binary on 2026-08-27. One codec,
//! one crate, two decoders: the node keeps the C-backed `zstd` (its replay path has a
//! 10 ms seek budget), the light client gets pure-Rust `ruzstd` behind the `pure` feature.

use serde::{de::DeserializeOwned, Serialize};

pub const REC_MAGIC: u8 = 0xB5;
pub const REC_VERSION_V1: u8 = 1;
pub const REC_VERSION_V2: u8 = 2;
/// `MAGIC | VERSION | height(8)` — the fixed part before the compressed body.
pub const REC_HEADER: usize = 10;
/// zstd level 3: measured 896 B/blk. Higher levels gain little on records this small and
/// cost append latency on the settlement path, which is the one place that must stay hot.
pub const REC_ZSTD_LEVEL: i32 = 3;
/// The trained v2 dictionary (64 KiB, zstd dictionary magic `37 A4 30 EC`). Bumping it is
/// a new record VERSION, never an in-place edit — every v2 record ever written names this
/// exact dictionary.
pub const CHAINLOG_DICT_V2: &[u8] = include_bytes!("../dict/chainlog-v2.zdict");

/// How many leading bytes of a legacy JSON record the skip-probe reads/searches.
/// `header.height` sits ~40-80 bytes in (header is the block's first field; only
/// `version` and the 8-byte `network_id` array precede it).
pub const PROBE_WINDOW: usize = 256;

/// Which form a payload is in, from its first byte alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    LegacyJson,
    V1,
    V2,
}

pub fn kind_of(payload: &[u8]) -> Option<RecordKind> {
    match payload.first()? {
        b'{' => Some(RecordKind::LegacyJson),
        &REC_MAGIC if payload.len() >= REC_HEADER => match payload[1] {
            REC_VERSION_V1 => Some(RecordKind::V1),
            REC_VERSION_V2 => Some(RecordKind::V2),
            _ => None,
        },
        _ => None,
    }
}

/// Height of a record, from its framing, without decompressing the body. Exact and O(1)
/// for v1/v2 (the height is stored in the record header precisely so a tail-replay skip
/// never decompresses a block it will discard); a byte-scan heuristic for legacy JSON that
/// returns `None` on any doubt — callers then fall back to a real parse, so a wrong or
/// missing probe can never change which blocks are applied.
pub fn probe_height(payload: &[u8]) -> Option<u64> {
    match kind_of(payload)? {
        RecordKind::V1 | RecordKind::V2 => Some(u64::from_le_bytes(payload[2..10].try_into().ok()?)),
        RecordKind::LegacyJson => probe_height_fast(payload),
    }
}

/// Fast height extraction for legacy JSON: the first `"height":<digits>` in the record's
/// leading bytes. `header` is the block's first field and `height` its third, so the first
/// occurrence IS `header.height` (the only other height-ish key, `"at_height"`, has no
/// quote before the `h` and can't match).
pub fn probe_height_fast(bytes: &[u8]) -> Option<u64> {
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
    // Digits must END inside the window — a digit run cut off by the window edge (probe
    // fed only a record prefix) would yield a truncated, too-small height and could skip a
    // block we must apply. Refuse instead.
    if any && terminated { Some(val) } else { None }
}

/// Split a v1/v2 record into `(version, height, compressed body)`.
pub fn split(payload: &[u8]) -> Option<(u8, u64, &[u8])> {
    match kind_of(payload)? {
        RecordKind::V1 | RecordKind::V2 => {
            let height = u64::from_le_bytes(payload[2..10].try_into().ok()?);
            Some((payload[1], height, &payload[REC_HEADER..]))
        }
        RecordKind::LegacyJson => None,
    }
}

/// The MessagePack body, decoded with a caller-supplied decompressor for v1 (plain zstd
/// frame) and v2 (frame compressed against [`CHAINLOG_DICT_V2`]). This is the shape both
/// decoder backends share; it is what makes "one codec, two decoders" a single reader.
pub fn decode_with<T, D1, D2>(payload: &[u8], zstd_plain: D1, zstd_dict: D2) -> Option<T>
where
    T: DeserializeOwned,
    D1: FnOnce(&[u8]) -> Option<Vec<u8>>,
    D2: FnOnce(&[u8]) -> Option<Vec<u8>>,
{
    match kind_of(payload)? {
        RecordKind::LegacyJson => serde_json::from_slice(payload).ok(),
        RecordKind::V1 => rmp_serde::from_slice(&zstd_plain(&payload[REC_HEADER..])?).ok(),
        RecordKind::V2 => rmp_serde::from_slice(&zstd_dict(&payload[REC_HEADER..])?).ok(),
    }
}

// ── C-backed zstd: the node's encoder and its fast decoder ──────────────────────────────
#[cfg(feature = "c-zstd")]
mod c {
    use super::*;

    thread_local! {
        static V2_COMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Compressor<'static>>> = const { std::cell::RefCell::new(None) };
        static V2_DECOMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { std::cell::RefCell::new(None) };
    }

    pub fn v2_compress(raw: &[u8]) -> std::io::Result<Vec<u8>> {
        V2_COMPRESSOR.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_none() {
                *c = Some(zstd::bulk::Compressor::with_dictionary(REC_ZSTD_LEVEL, CHAINLOG_DICT_V2)?);
            }
            c.as_mut().expect("just set").compress(raw)
        })
    }

    pub fn v2_decompress(body: &[u8]) -> std::io::Result<Vec<u8>> {
        V2_DECOMPRESSOR.with(|d| {
            let mut d = d.borrow_mut();
            if d.is_none() {
                *d = Some(zstd::bulk::Decompressor::with_dictionary(CHAINLOG_DICT_V2)?);
            }
            // `bulk::Decompressor::decompress` ALLOCATES AND ZERO-FILLS `capacity` bytes up
            // front, so a lazy 64 MiB ceiling here cost ~120 ms per record (MEASURED: the
            // replay tests' 10 ms seek budget blew to 123 ms). The frame carries its own
            // content size — use it, and fall back to a bounded multiple of the compressed
            // length if it doesn't.
            let cap = zstd::zstd_safe::get_frame_content_size(body)
                .ok()
                .flatten()
                .map(|n| n as usize)
                .unwrap_or(body.len().saturating_mul(16))
                .clamp(1024, 64 << 20);
            d.as_mut().expect("just set").decompress(body, cap)
        })
    }

    /// Encode a block into a record payload. **The single encoder** — every writer goes
    /// through here so a format change can never be applied to only some call sites.
    ///
    /// Escape hatches (reading never needs them — every form always decodes):
    /// `SIGIL_CHAINLOG_JSON=1` writes legacy JSON, `SIGIL_CHAINLOG_V1=1` writes v1 — for
    /// bisecting a suspected dictionary/codec bug against a known-good reader.
    pub fn encode_record<T: Serialize>(height: u64, block: &T) -> std::io::Result<Vec<u8>> {
        if std::env::var("SIGIL_CHAINLOG_JSON").as_deref() == Ok("1") {
            return serde_json::to_vec(block)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
        let packed = rmp_serde::to_vec_named(block)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let v1 = std::env::var("SIGIL_CHAINLOG_V1").as_deref() == Ok("1");
        let body = if v1 { zstd::encode_all(&packed[..], REC_ZSTD_LEVEL)? } else { v2_compress(&packed)? };
        let mut out = Vec::with_capacity(REC_HEADER + body.len());
        out.push(REC_MAGIC);
        out.push(if v1 { REC_VERSION_V1 } else { REC_VERSION_V2 });
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode a record payload written in ANY form. Returns `None` rather than panicking on
    /// a torn or unknown record: every caller treats a failed decode as "stop the scan
    /// here", which is correct for an append-only log whose tail may be a partial write.
    pub fn decode_record<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
        decode_with(
            payload,
            |b| zstd::decode_all(b).ok(),
            |b| v2_decompress(b).ok(),
        )
    }
}
#[cfg(feature = "c-zstd")]
pub use c::{decode_record, encode_record, v2_compress, v2_decompress};

// ── Pure-Rust zstd: the light client's decoder (no C in the Windows cross-build) ─────────
// `test` is included so a plain `cargo test -p sigil-record` (no `--features pure`) still
// compiles the cross-decoder tests below — the dev-dependency on ruzstd is always linked in
// test builds. Found 2026-09-14: the default-feature test run failed with E0433 on `pure::`.
#[cfg(any(feature = "pure", test))]
pub mod pure {
    use super::*;
    use std::io::Read;

    /// A zstd-bomb guard: a gossiped block record must never inflate past this.
    pub const MAX_OUT: u64 = 64 * 1024 * 1024;

    fn inflate(body: &[u8], dict: Option<&[u8]>) -> Option<Vec<u8>> {
        let mut fd = ruzstd::FrameDecoder::new();
        if let Some(d) = dict {
            fd.add_dict(ruzstd::decoding::dictionary::Dictionary::decode_dict(d).ok()?).ok()?;
        }
        let dec = ruzstd::StreamingDecoder::new_with_decoder(body, fd).ok()?;
        let mut out = Vec::new();
        dec.take(MAX_OUT + 1).read_to_end(&mut out).ok()?;
        if out.len() as u64 > MAX_OUT { return None; }
        Some(out)
    }

    /// Decode a record payload (legacy JSON, v1, v2) with the pure-Rust decoder.
    pub fn decode_record<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
        decode_with(payload, |b| inflate(b, None), |b| inflate(b, Some(CHAINLOG_DICT_V2)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Header { version: u8, network_id: [u8; 8], height: u64, hash: Vec<u8> }
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Block { header: Header, events: Vec<Ev>, txs: Vec<u32> }
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    // flux-wire: allow — test fixture: the internally-tagged shape this codec exists to carry (never bincode)
    #[serde(tag = "kind")]
    enum Ev { Send { amount: u64 }, Order { name: String } }

    fn block(h: u64) -> Block {
        Block {
            header: Header { version: 0, network_id: *b"sigil-g2", height: h, hash: vec![7; 32] },
            events: vec![Ev::Send { amount: 5 }, Ev::Order { name: "Ridder".into() }],
            txs: (0..40).collect(),
        }
    }

    #[test]
    fn framing_and_probe() {
        let rec = encode_record(12345, &block(12345)).unwrap();
        assert_eq!(kind_of(&rec), Some(RecordKind::V2));
        assert_eq!(probe_height(&rec), Some(12345));
        assert_eq!(split(&rec).map(|(v, h, _)| (v, h)), Some((REC_VERSION_V2, 12345)));
        let json = serde_json::to_vec(&block(7)).unwrap();
        assert_eq!(kind_of(&json), Some(RecordKind::LegacyJson));
        assert_eq!(probe_height(&json), Some(7));
        assert_eq!(kind_of(&[REC_MAGIC, 99, 0, 0, 0, 0, 0, 0, 0, 0]), None, "bad version");
        assert_eq!(kind_of(&[REC_MAGIC, REC_VERSION_V1, 1, 2]), None, "truncated header");
        assert_eq!(CHAINLOG_DICT_V2.len(), 65536);
        assert_eq!(&CHAINLOG_DICT_V2[..4], &[0x37, 0xA4, 0x30, 0xEC], "zstd dictionary magic");
    }

    /// The point of the crate: the C encoder's bytes decode identically through the C
    /// decoder AND the pure-Rust one, for every record form, internally-tagged enums
    /// included (the thing bincode could not do).
    #[test]
    fn both_decoders_read_every_form() {
        let b = block(99);
        let v2 = encode_record(99, &b).unwrap();
        let packed = rmp_serde::to_vec_named(&b).unwrap();
        let mut v1 = vec![REC_MAGIC, REC_VERSION_V1];
        v1.extend_from_slice(&99u64.to_le_bytes());
        v1.extend_from_slice(&zstd::encode_all(&packed[..], REC_ZSTD_LEVEL).unwrap());
        let json = serde_json::to_vec(&b).unwrap();
        for (name, rec) in [("v2", &v2), ("v1", &v1), ("json", &json)] {
            assert_eq!(decode_record::<Block>(rec).as_ref(), Some(&b), "c-zstd {name}");
            assert_eq!(pure::decode_record::<Block>(rec).as_ref(), Some(&b), "ruzstd {name}");
        }
        assert!(v2.len() < v1.len() && v1.len() < json.len(), "v2 {} < v1 {} < json {}", v2.len(), v1.len(), json.len());
    }

    /// A reader that wants only the header can decode a v2 record into a narrower struct,
    /// and into a `serde_json::Value` — MessagePack keeps the names, so the light client
    /// needs neither the node's `Block` type nor a JSON detour.
    #[test]
    fn partial_and_dynamic_reads() {
        #[derive(Deserialize)]
        struct HeaderOnly { header: Header }
        let rec = encode_record(5, &block(5)).unwrap();
        let ho: HeaderOnly = pure::decode_record(&rec).unwrap();
        assert_eq!(ho.header.height, 5);
        let v: serde_json::Value = pure::decode_record(&rec).unwrap();
        assert_eq!(v["header"]["height"].as_u64(), Some(5));
        assert_eq!(v["events"][1]["kind"], "Order");
    }

    #[test]
    fn torn_and_bogus_records_are_none_not_panics() {
        let rec = encode_record(3, &block(3)).unwrap();
        assert!(decode_record::<Block>(&rec[..rec.len() / 2]).is_none());
        assert!(pure::decode_record::<Block>(&rec[..rec.len() / 2]).is_none());
        assert!(decode_record::<Block>(b"\x00garbage").is_none());
        assert!(pure::decode_record::<Block>(b"{not json").is_none());
    }
}
