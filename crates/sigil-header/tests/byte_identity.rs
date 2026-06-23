// LANE 3 (THROUGHPUT_MASTER.md, 2026-06-23) — BYTE-IDENTITY PROOF
// ================================================================
//
// The fast-sync throughput work collapses the **3 bincode round-trips per record**
// on the snapshot-pull hot path into one pass (decode → verify → commit):
//
//   OLD (3 trips/record):
//     1. DECODE   page bytes  -> Vec<SkeletonRecord>          (bincode::deserialize)
//     2. VERIFY   bincode::serialize(rec) per record, fold into BLAKE3 archive_root
//     3. COMMIT   bincode::serialize(rec) per record, write to the flat store
//
//   NEW (one pass over the raw page body):
//     - parse each record as a fixed 72-byte stride (no Vec alloc, no bincode machinery)
//     - VERIFY  folds the RAW 72 bytes straight into BLAKE3 (fetch.rs SnapshotVerifier::push)
//     - COMMIT  appends the RAW 72-byte slices straight through (on-disk == wire bytes)
//
// This is only sound — and the chain stays consensus-identical — IFF the bincode wire
// layout is exactly the flat little-endian record we hash/append directly. INVARIANT §5.1
// ("consensus block hash immutable — cache it, never change its bytes") and the
// `archive_root` definition in sigil-header (BLAKE3 over the canonical bincode of every
// SkeletonRecord, in order) both ride on the equalities proven below.
//
// We assert against `bincode = "1"` (1.3.3) — the SAME crate sigil-top serializes the wire
// with (crates/sigil-top/Cargo.toml:46) — so this proves byte-identity vs the *current*
// shipped path, not vs an assumed layout.
//
// Companion proof for the OTHER half of LANE 3 — the cached `StoredBlock.hash_hex` so
// `SigilBlockHeaderV0::hash()` (JSON serialize, lib.rs:245-254) runs ONCE at ingest — is
// covered by construction (block_store.rs:489 sets `hash_hex: hex::encode(header.hash())`)
// plus the existing `tests::hash_is_deterministic` (lib.rs:419). See the bottom helper here
// for the field-by-field rationale; no header rebuild needed.

use sigil_header::SkeletonRecord;

/// Build a record with maximally-distinct bytes so any field swap/endianness slip is caught.
fn rec(height: u64, bh: u8, ph: u8) -> SkeletonRecord {
    SkeletonRecord { height, block_hash: [bh; 32], parent_hash: [ph; 32] }
}

/// The flat 72-byte canonical layout the verify/commit fast paths assume:
/// `height(8 LE) ‖ block_hash(32) ‖ parent_hash(32)`.
fn raw72(r: &SkeletonRecord) -> Vec<u8> {
    let mut v = Vec::with_capacity(72);
    v.extend_from_slice(&r.height.to_le_bytes());
    v.extend_from_slice(&r.block_hash);
    v.extend_from_slice(&r.parent_hash);
    v
}

/// #1 — `bincode(SkeletonRecord)` IS the flat 72-byte little-endian layout.
/// This is the keystone: it makes `SnapshotVerifier::push` hashing raw fields (fetch.rs:442-444)
/// byte-identical to the old `hasher.update(&bincode::serialize(rec))`, and makes a raw-slice
/// commit byte-identical to a re-encode.
#[test]
fn skeleton_record_bincode_is_flat_72_le() {
    // A height with every byte distinct exposes endianness (LE vs BE) immediately.
    let r = rec(0x0102_0304_0506_0708, 0xAB, 0xCD);
    let enc = bincode::serialize(&r).expect("bincode encode");

    assert_eq!(enc.len(), 72, "record must be exactly 72 bytes on the wire");
    assert_eq!(enc, raw72(&r), "bincode bytes must equal height_le ‖ block_hash ‖ parent_hash");

    // Spell out the field offsets the stride parser relies on.
    assert_eq!(&enc[0..8], &r.height.to_le_bytes(), "height = first 8 bytes, little-endian");
    assert_eq!(&enc[8..40], &r.block_hash, "block_hash = bytes 8..40, raw (no len prefix)");
    assert_eq!(&enc[40..72], &r.parent_hash, "parent_hash = bytes 40..72, raw (no len prefix)");
}

/// #2 — `bincode(Vec<SkeletonRecord>)` (an `'S'` page payload) IS `count(8 LE) ‖ N×72B`.
/// This is what lets `pull_snapshot` walk a page in one stride loop instead of
/// `bincode::deserialize::<Vec<SkeletonRecord>>` (fetch.rs:556-557).
#[test]
fn page_bincode_is_len_prefix_plus_contiguous_strides() {
    let recs: Vec<SkeletonRecord> =
        (0..7u64).map(|i| rec(i, i as u8, (i as u8).wrapping_sub(1))).collect();
    let page = bincode::serialize(&recs).expect("bincode encode page");

    assert_eq!(page.len(), 8 + recs.len() * 72, "page = u64 count ‖ N×72");
    assert_eq!(&page[0..8], &(recs.len() as u64).to_le_bytes(), "count prefix is u64 LE");

    for (i, r) in recs.iter().enumerate() {
        let off = 8 + i * 72;
        assert_eq!(&page[off..off + 72], raw72(r).as_slice(), "record {i} stride matches raw72");
    }
}

/// #3 — A one-pass stride parser over the raw page body reconstructs records BYTE-IDENTICAL
/// to `bincode::deserialize` (and to the originals). Proves the decode trip can be replaced
/// with a Vec-free stride walk with zero semantic change.
#[test]
fn one_pass_stride_parse_equals_bincode_deserialize() {
    let recs: Vec<SkeletonRecord> =
        (10..23u64).map(|i| rec(i * 1000 + 7, (i * 3) as u8, (i * 5) as u8)).collect();
    let page = bincode::serialize(&recs).expect("encode");

    let decoded: Vec<SkeletonRecord> = bincode::deserialize(&page).expect("decode");

    // The exact parser pull_snapshot can use in place of bincode::deserialize:
    let count = u64::from_le_bytes(page[0..8].try_into().unwrap()) as usize;
    let body = &page[8..];
    assert_eq!(body.len(), count * 72, "body is exactly count×72");
    let mut parsed = Vec::with_capacity(count);
    for i in 0..count {
        let s = &body[i * 72..i * 72 + 72];
        let height = u64::from_le_bytes(s[0..8].try_into().unwrap());
        let mut block_hash = [0u8; 32];
        block_hash.copy_from_slice(&s[8..40]);
        let mut parent_hash = [0u8; 32];
        parent_hash.copy_from_slice(&s[40..72]);
        parsed.push(SkeletonRecord { height, block_hash, parent_hash });
    }

    assert_eq!(parsed, decoded, "stride parse == bincode::deserialize");
    assert_eq!(parsed, recs, "stride parse == originals");
}

/// #4 — THE CONSENSUS-CRITICAL ONE. The BLAKE3 `archive_root` is INVARIANT across all three
/// representations: (a) old per-record `bincode::serialize`, (b) the new per-record raw-field
/// fold (`SnapshotVerifier::push`), (c) hashing the contiguous raw page body in one shot
/// (what a streaming verifier/commit can do). Identical root ⇒ the `trailer.archive_root`
/// check (`SnapshotVerifier::finalize`) passes identically ⇒ NO verify/consensus change.
#[test]
fn archive_root_is_invariant_across_all_three_representations() {
    let recs: Vec<SkeletonRecord> =
        (0..64u64).map(|i| rec(i, (i ^ 0x5a) as u8, (i ^ 0xa5) as u8)).collect();

    // (a) OLD: bincode::serialize(rec) per record, in order.
    let root_old = {
        let mut h = blake3::Hasher::new();
        for r in &recs {
            h.update(&bincode::serialize(r).unwrap());
        }
        *h.finalize().as_bytes()
    };

    // (b) NEW verify: raw fields per record (mirrors fetch.rs SnapshotVerifier::push exactly).
    let root_push = {
        let mut h = blake3::Hasher::new();
        for r in &recs {
            h.update(&r.height.to_le_bytes());
            h.update(&r.block_hash);
            h.update(&r.parent_hash);
        }
        *h.finalize().as_bytes()
    };

    // (c) NEW streaming: hash the contiguous raw page body in ONE update (BLAKE3 is a stream,
    // so chunk boundaries are irrelevant — the bytes are what matter).
    let root_stream = {
        let page = bincode::serialize(&recs).unwrap();
        let mut h = blake3::Hasher::new();
        h.update(&page[8..]); // skip the u64 count prefix; the body is concat of the 72-B records
        *h.finalize().as_bytes()
    };

    assert_eq!(root_old, root_push, "raw-field fold == per-record bincode fold");
    assert_eq!(root_old, root_stream, "whole-body stream == per-record bincode fold");
}

/// #5 — `from_header().block_hash` IS `header.hash()`, so a SkeletonRecord carries the
/// consensus hash verbatim and the linkage walk never needs to recompute the ~3-5 KB JSON
/// hash. (Direct check of `from_header` would need a full header rebuild; here we lock the
/// downstream guarantee that the skeleton record's `block_hash` is a 32-byte content hash
/// that bincode round-trips losslessly — the property the cached `StoredBlock.hash_hex` and
/// the linkage compare both stand on.)
#[test]
fn record_block_hash_roundtrips_losslessly() {
    let r = rec(7, 0x11, 0x22);
    let back: SkeletonRecord = bincode::deserialize(&bincode::serialize(&r).unwrap()).unwrap();
    assert_eq!(back.block_hash, r.block_hash, "block_hash survives encode/decode unchanged");
    assert_eq!(back, r, "full record survives encode/decode unchanged");
}
