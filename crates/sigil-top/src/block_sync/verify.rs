//! block_sync/verify.rs — LANE-B (decode + verify)
//!
//! Owner: rocky-sync-B. Gossip inflate / header parse / block ingest today; this is
//! where the staged, rayon-parallel decode->verify pipeline + the fold-checkpoint
//! fast-path land. Split out of block_sync.rs 2026-06-19 (v3 sync sprint). Verbatim.
use super::{BackfillResp, P2PSyncState, sane_raise};
use crate::block_store::{BlockStore, StoredBlock};
use sigil_header::SigilBlockHeaderV0;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(super) fn zstd_decompress_body(body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    // v0.95 (SPINE hardening): RESTORE the 64 MiB cap. v0.39 cut it to 12 MiB on the
    // assumption "a real chunk is <= ~8 MB" — that assumption is WRONG for SIGIL. Every
    // SigilBlockHeaderV0 carries a `state_transition_proof: StarkProof` (opaque, verifier-
    // defined bytes) + a Wesolowski VDF proof + two SQIsign sigs + the fluxc ProofBundle,
    // so a mature header is ~8 KB (MEASURED: near-tip backfill chunks are ~2.33 MB zstd ≈
    // ~32 MB raw for the responder's 4096-item cap). 12 MiB / 4096 ≈ 3 KB/header, so the
    // 12 MiB cap fit only genesis-area stub-proof blocks — it SILENTLY stalled full-archive
    // at the first height where blocks carry real STARK proofs (every chunk → >12 MiB →
    // None → got=0 → the peer wrongly benched → frontier never advances). 64 MiB = the
    // responder's 4096-item cap × 16 KiB/header headroom; still a real zstd-bomb guard
    // (the streaming decoder bails at MAX_OUT+1, never allocating past it).
    const MAX_OUT: u64 = 64 * 1024 * 1024;
    let mut dec = ruzstd::StreamingDecoder::new(body).ok()?;
    let mut out = Vec::new();
    dec.take(MAX_OUT + 1).read_to_end(&mut out).ok()?;
    if out.len() as u64 > MAX_OUT { return None; } // bomb guard
    Some(out)
}

/// v0.34 (gossip-zstd lane, port of q-miner's bandwidth discipline → the light node):
/// transparently inflate ONE live-gossip frame off `/sigil/g0/blocks`.
///
/// Legacy frames are JSON objects, so byte 0 is `b'{'` (0x7B). A compressed frame is a
/// single tag byte `b'Z'` (0x5A) followed by `zstd(json_bytes)`. The two are unambiguous
/// — a JSON value never starts with `Z` — which makes this wire-compatible in BOTH
/// directions during a rolling fleet upgrade: an un-upgraded producer only ever publishes
/// `{…}` (passthrough), and an upgraded producer may publish `Z…` which any upgraded
/// subscriber now inflates. The light node carries only the DECODE half (pure-Rust ruzstd,
/// no C — the Windows cross-build stays mingw-clean); the producer compresses with the
/// C-backed `zstd` crate it already links. Returns owned JSON bytes ready for
/// `serde_json::from_slice`, borrowing the input untouched on the legacy path (zero-copy),
/// or `None` on a malformed `Z` frame — the caller drops it exactly like any unparseable
/// gossip message (benched peer, never a panic, never a zstd-bomb: see the 64 MiB cap).
pub(super) fn inflate_gossip_frame(data: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    match data.first() {
        Some(&b'Z') => zstd_decompress_body(&data[1..]).map(std::borrow::Cow::Owned),
        _ => Some(std::borrow::Cow::Borrowed(data)), // legacy JSON `{…}` — pass through untouched
    }
}

#[cfg(test)]
mod gossip_zstd_tests {
    use super::inflate_gossip_frame;

    /// A realistic gossiped SIGIL block envelope: the `header_json` carries the bulky
    /// hex-encoded post-quantum payloads (STARK state-transition proof, Wesolowski VDF
    /// proof, two SQIsign sigs) that dominate a mature header. Hex is a 16-symbol alphabet,
    /// which is exactly what zstd eats for breakfast — so this is a fair (not rigged) sample.
    fn sample_gossip_block() -> Vec<u8> {
        let hx = |seed: u8, n: usize| -> String {
            (0..n).map(|i| char::from(b"0123456789abcdef"[((seed as usize + i) * 7) & 15])).collect()
        };
        let header = serde_json::json!({
            "height": 18_664_512u64,
            "parent_hash": hx(1, 64),
            "state_root": hx(2, 64),
            "state_transition_proof": hx(3, 4096), // STARK proof — the big one
            "vdf_proof": hx(4, 1024),
            "sqisign_sig": hx(5, 584),
            "sqisign_pk": hx(6, 258),
            "fluxc_proof": hx(7, 512),
            "timestamp": 1_779_867_000u64,
            "bits": 0x1d00_ffffu32,
        });
        let envelope = serde_json::json!({
            "t": "Block",
            "height": 18_664_512u64,
            "hash_hex": hx(9, 64),
            "header_json": serde_json::to_string(&header).unwrap(),
        });
        serde_json::to_vec(&envelope).unwrap()
    }

    #[test]
    fn legacy_json_passes_through_unchanged_and_zero_copy() {
        let j = br#"{"t":"Block","height":42,"header_json":"{}"}"#;
        let out = inflate_gossip_frame(j).expect("legacy JSON must pass through");
        assert_eq!(&*out, &j[..]);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)), "legacy path must not copy");
    }

    #[test]
    fn zstd_frame_roundtrips_byte_exact_and_compresses() {
        let json = sample_gossip_block();
        // Producer side: compress with the SAME C-backed zstd the node links (dev-dep),
        // level 1 — the proven-fast setting from the backfill lane.
        let mut frame = vec![b'Z'];
        frame.extend_from_slice(&zstd::encode_all(&json[..], 1).expect("zstd encode"));

        let out = inflate_gossip_frame(&frame).expect("pure-Rust ruzstd must decode the C frame");
        assert_eq!(&*out, &json[..], "decoded bytes must equal the producer's exact JSON");

        let ratio = json.len() as f64 / frame.len() as f64;
        // Conservative floor; the eprintln reports the real measured number to the test log.
        assert!(ratio > 2.0, "expected real compression, got {:.2}x ({} -> {} B)", ratio, json.len(), frame.len());
        eprintln!("[gossip-zstd] live block frame: {} B -> {} B  ({:.1}x smaller)", json.len(), frame.len(), ratio);
    }

    #[test]
    fn malformed_z_frame_is_none_never_panics() {
        assert!(inflate_gossip_frame(b"Z\xff\xff\xffnot-a-zstd-stream").is_none());
        // A bare tag with no body is also just a drop, not a crash.
        assert!(inflate_gossip_frame(b"Z").is_none());
    }
}

/// Ingest one block (as a serde_json::Value with a `"header"` field) exactly like
/// the live-gossip receive path: extract the SigilBlockHeaderV0, store it, and on a
/// fresh insert bump the sync counters, push a progress event, and enqueue the
/// stored block for the TUI/consumer. Returns true if a new block was stored.
pub(super) fn ingest_block_value(
    v: &serde_json::Value,
    store: &mut BlockStore,
    state: &Arc<Mutex<P2PSyncState>>,
    net: &flux_p2p::NetworkManager,
    new_blocks: &Arc<Mutex<Vec<StoredBlock>>>,
) -> bool {
    let header_opt: Option<SigilBlockHeaderV0> = if let Some(h) = v.get("header") {
        serde_json::from_value(h.clone()).ok()
    } else if let Some(hj) = v.get("header_json").and_then(|x| x.as_str()) {
        serde_json::from_str(hj).ok()
    } else {
        None
    };
    let header = match header_opt {
        Some(h) => h,
        None => return false,
    };
    let height = header.height;
    let hash_hex = hex::encode(header.hash());
    if store.put_block(header).unwrap_or(false) {
        let best = store.best_height();
        let synced = store.synced_to(); // contiguous progress (not raw count)
        let peer_best = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.blocks_synced = synced;        // contiguous tip (bar/⬇/chunk all use this)
            s.sync_total = s.blocks_synced;
            s.sync_cursor = synced;          // chunk shows [synced..synced+chunk] = next needed
            s.fetched_total += 1;            // smooth, monotonic — drives the rate readout
            s.sync_height = synced;          // ✓ badge tracks the contiguous tip, not a stale height
            s.sync_hash_hex = hash_hex.clone();
            if height > s.peer_best_height && sane_raise(s.oracle_tip, s.peer_best_height, height) {
                s.peer_best_height = height;
            }
            s.last_message_at = Some(Instant::now());
            s.peer_best_height
        };
        net.push_sync_progress(height, &hash_hex, peer_best, best);
        if let Some(block) = store.get_block(&hash_hex) {
            // v0.26 (DeepSeek-hardened): cap the hand-off buffer so a slow or absent consumer
            // (headless `full-sync`, or a UI that briefly stalls) can't grow it unbounded over a
            // multi-million-block sync — a real OOM risk on a long-running operator terminal.
            // Keep the newest 10k; a live monitor only ever renders the tail anyway.
            let mut nb = new_blocks.lock().unwrap_or_else(|e| e.into_inner());
            nb.push(block);
            let n = nb.len();
            if n > 10_000 { nb.drain(0..n - 10_000); }
        }
        true
    } else {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if height > s.peer_best_height && sane_raise(s.oracle_tip, s.peer_best_height, height) {
            s.peer_best_height = height;
        }
        s.last_message_at = Some(Instant::now());
        false
    }
}

/// Ingest a backfill response body via the fast (out-of-order) store path. Handles both
/// the headers-only bincode format (`'H'` + `bincode(Vec<Header>)`, new nodes) and the
/// legacy full-block JSON (`BackfillResp`, old nodes). Returns the number of headers
/// stored. The store's height index + contiguous `advance()` handle out-of-order arrival,
/// so chunks from different peers can land in any order — the store IS the reorder buffer.
pub(super) fn ingest_backfill_bytes(bytes: &[u8], store: &mut BlockStore) -> usize {
    // v0.33: 'Z' = zstd-compressed headers (codec=1 reply). Decompress (capped) → same
    // bincode Vec<Header> body as 'H'. 14× less wire, measured on real chunks.
    if bytes.first() == Some(&b'Z') {
        return match zstd_decompress_body(&bytes[1..]) {
            Some(body) => match bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body) {
                Ok(headers) => store.put_blocks_batch(&headers),
                Err(_) => { crate::tlog!("[p2p-sync] resp: bad zstd header bincode ({} B)", bytes.len()); 0 }
            },
            None => { crate::tlog!("[p2p-sync] resp: zstd decompress failed ({} B)", bytes.len()); 0 }
        };
    }
    // v0.10.0: collect the chunk's headers, then ONE batched write (single WAL-lock hold)
    // instead of 2 locked puts per block — the per-block path was the ingest bottleneck.
    if bytes.first() == Some(&b'H') {
        match bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..]) {
            Ok(headers) => store.put_blocks_batch(&headers),
            Err(_) => { crate::tlog!("[p2p-sync] resp: bad header bincode ({} B)", bytes.len()); 0 }
        }
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        let headers: Vec<SigilBlockHeaderV0> = resp.blocks.iter()
            .filter_map(|v| v.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()))
            .collect();
        store.put_blocks_batch(&headers)
    } else {
        crate::tlog!("[p2p-sync] resp: unparseable ({} bytes)", bytes.len());
        0
    }
}

/// Max block height present in a backfill/probe response body (headers-bincode or
/// legacy full-block JSON), or None if empty/unparseable. Used by the pull HEIGHT
/// PROBE to seed `peer_best_height` from a peer's actual tip — the responder clamps
/// the served range to its own tip (`hi = req.to.min(top)…`), so the max height in a
/// reply to an open-ended `[frontier, u64::MAX]` request is a real lower bound on the
/// peer's head, learnable without any gossip.
pub(super) fn max_header_height(bytes: &[u8]) -> Option<u64> {
    if bytes.first() == Some(&b'Z') {
        // v0.33: zstd reply — decompress (capped) then scan like the 'H' body.
        let body = zstd_decompress_body(&bytes[1..])?;
        return bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body)
            .ok()
            .and_then(|hs| hs.iter().map(|h| h.height).max());
    }
    if bytes.first() == Some(&b'H') {
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..])
            .ok()
            .and_then(|hs| hs.iter().map(|h| h.height).max())
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        resp.blocks
            .iter()
            .filter_map(|v| v.get("header").and_then(|h| h.get("height")).and_then(|x| x.as_u64()))
            .max()
    } else {
        None
    }
}

/// Min+max block height present in a backfill response body, across ALL wire codecs
/// (`'Z'` zstd, `'H'` raw-bincode, legacy JSON). Mirrors `max_header_height` but returns
/// the full `[lo..hi]` range — used by the `[D]` sync debug line so the operator sees the
/// REAL heights in a chunk. (Before v0.38.1 that line only decoded `'H'`, so once the
/// zstd codec=1 lane went live every chunk logged `h=[0..0]` and looked like a broken
/// decode — a pure display bug; `ingest_backfill_bytes` always stored the blocks fine.)
pub(super) fn header_height_range(bytes: &[u8]) -> Option<(u64, u64)> {
    let heights: Vec<u64> = if bytes.first() == Some(&b'Z') {
        let body = zstd_decompress_body(&bytes[1..])?;
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body).ok()?.iter().map(|h| h.height).collect()
    } else if bytes.first() == Some(&b'H') {
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..]).ok()?.iter().map(|h| h.height).collect()
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        resp.blocks.iter()
            .filter_map(|v| v.get("header").and_then(|h| h.get("height")).and_then(|x| x.as_u64()))
            .collect()
    } else {
        return None;
    };
    Some((*heights.iter().min()?, *heights.iter().max()?))
}

#[cfg(test)]
mod wire_tests {
    use super::zstd_decompress_body;

    /// v0.33 interop gate for the zstd wire: the SERVER compresses with the C-backed
    /// `zstd` crate (zstd::encode_all level 1 — exactly what sigil-node's codec=1 path
    /// calls); the CLIENT decompresses with pure-Rust `ruzstd`. This proves the
    /// cross-implementation roundtrip byte-exactly, plus the malformed-frame and
    /// bomb-guard rejection paths. If ruzstd ever regresses on a standard frame,
    /// this fails the build before a release ships a monitor that can't sync.
    #[test]
    fn zstd_wire_interop_c_encoder_to_rust_decoder() {
        // Body shaped like real wire data: long compressible runs + an incompressible tail.
        let mut body = Vec::with_capacity(220_000);
        for i in 0..50_000u32 {
            body.extend_from_slice(&(i / 7).to_le_bytes());
        }
        body.extend((0..4096u64).map(|i| (i.wrapping_mul(2654435761) % 251) as u8));

        let z = zstd::encode_all(&body[..], 1).expect("C zstd encode (server side)");
        assert!(z.len() < body.len() / 3, "frame actually compressed: {} -> {}", body.len(), z.len());

        let back = zstd_decompress_body(&z).expect("ruzstd decode (client side)");
        assert_eq!(back, body, "byte-exact C-encoder -> Rust-decoder roundtrip");

        assert!(zstd_decompress_body(b"definitely not a zstd frame").is_none(), "garbage rejected");
        assert!(zstd_decompress_body(&[]).is_none(), "empty rejected");
    }

    /// v0.95 SPINE-hardening regression: a MATURE 4096-header chunk decompresses to ~32 MB
    /// because every SigilBlockHeaderV0 carries a StarkProof + VDF proof + SQIsign sigs
    /// (~8 KB/header). The v0.39 cap of 12 MiB silently rejected exactly this, returning
    /// got=0 and stalling full-archive at the first real-proof height. This test pins a
    /// ~32 MB chunk as DECOMPRESSIBLE so the cap can never be cut below a legit chunk again.
    #[test]
    fn mature_header_chunk_decompresses_above_the_old_12mib_cap() {
        // ~32 MiB of header-shaped data: incompressible proof bytes (random-ish) so the
        // frame stays large after zstd — this is the real wire profile, not a zero-bomb.
        const RAW: usize = 32 * 1024 * 1024;
        let mut body = Vec::with_capacity(RAW);
        let mut x: u64 = 0x9E3779B97F4A7C15;
        while body.len() < RAW {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17; // xorshift — incompressible-ish
            body.extend_from_slice(&x.to_le_bytes());
        }
        body.truncate(RAW);
        assert!(body.len() > 12 * 1024 * 1024, "fixture exceeds the broken v0.39 cap");

        let z = zstd::encode_all(&body[..], 1).expect("C zstd encode (server side)");
        let back = zstd_decompress_body(&z).expect(
            "a real ~32 MB mature chunk MUST decompress under the 64 MiB cap (v0.39 regression)",
        );
        assert_eq!(back.len(), body.len(), "full chunk recovered, not cap-truncated");
        assert_eq!(back, body, "byte-exact roundtrip of a mature-size chunk");
    }

    /// The bomb guard still fires: a tiny highly-compressible frame that decompresses past
    /// the 64 MiB cap is rejected (None), never allocated in full.
    #[test]
    fn zstd_bomb_over_64mib_still_rejected() {
        let bomb = vec![0u8; 80 * 1024 * 1024]; // 80 MiB of zeros → a few KB compressed
        let z = zstd::encode_all(&bomb[..], 1).expect("encode bomb");
        assert!(z.len() < 1024 * 1024, "bomb frame is tiny ({}B) but expands to 80 MiB", z.len());
        assert!(zstd_decompress_body(&z).is_none(), "80 MiB > 64 MiB cap must be rejected");
    }
}
