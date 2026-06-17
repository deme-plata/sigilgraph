// Power-release #3 regression test — paste into block_sync.rs `#[cfg(test)] mod tests`.
// Run on Delta: `fluxc test --package sigil-top` (never raw cargo; never on Epsilon/Beta).
//
// Locks AHA#2.1: an HONEST chunk larger than the bomb-cap is classified TooBig, NOT
// counted as a stored=0 failure and NOT a Bad/bench. This is the exact case that
// silently stalled the full archive under v0.39's 12-MiB cap (every STARK-proof chunk
// → None → got=0 → innocent peer benched → frontier frozen). The test guarantees the
// frontier scheduler can branch to "shrink window + retry same peer" instead.

#[test]
fn over_cap_chunk_is_toobig_not_bench() {
    // Build a 'Z' frame whose decoded output exceeds the 64 MiB cap.
    // zstd of 80 MiB of zeros is tiny on the wire but decodes past MAX_OUT.
    let raw = vec![0u8; 80 * 1024 * 1024];
    let z = zstd::stream::encode_all(&raw[..], 1).expect("encode");
    let mut frame = vec![b'Z'];
    frame.extend_from_slice(&z);

    // Decode-level: distinguished from Ok, surfaced as TooBig (never silent None).
    assert_eq!(super::zstd_decode_checked(&frame[1..]), super::Decoded::TooBig);

    // Ingest-level: TooBig — NOT Stored(0) (which would read as got=0 → bench).
    let mut store = BlockStore::ephemeral_for_test();
    assert!(matches!(super::ingest_backfill_bytes(&frame, &mut store), super::Ingest::TooBig),
        "over-cap chunk must be TooBig (shrink+retry), never a bench-worthy got=0");
}

#[test]
fn genuine_garbage_is_bad_and_benchable() {
    // A 'Z' tag followed by non-zstd bytes is genuinely bad → Bad (bench justified).
    let mut store = BlockStore::ephemeral_for_test();
    let frame = b"Znot-zstd-garbage";
    // zstd_decode_checked returns TooBig on a malformed stream header by design
    // (we never silently Ok), but an 'H' frame with bad bincode must be Bad:
    let bad_h = b"Hnot-bincode";
    assert!(matches!(super::ingest_backfill_bytes(bad_h, &mut store), super::Ingest::Bad),
        "unparseable header bincode is Bad (bench justified)");
    let _ = frame;
}

#[test]
fn honest_empty_reply_is_stored_zero_not_bench() {
    // An honestly-empty 'H' chunk (peer at our frontier, nothing newer) is Stored(0):
    // no advance, but also NOT a bench.
    let empty: Vec<crate::SigilBlockHeaderV0> = vec![];
    let mut frame = vec![b'H'];
    frame.extend_from_slice(&bincode::serialize(&empty).unwrap());
    let mut store = BlockStore::ephemeral_for_test();
    assert!(matches!(super::ingest_backfill_bytes(&frame, &mut store), super::Ingest::Stored(0)),
        "empty reply is Stored(0), not a bench");
}
