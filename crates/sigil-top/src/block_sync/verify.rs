//! block_sync/verify.rs — LANE-B (decode + verify)
//!
//! Owner: rocky-sync-B. Gossip inflate / header parse / block ingest today; this is
//! where the staged, rayon-parallel decode->verify pipeline + the fold-checkpoint
//! fast-path land. Split out of block_sync.rs 2026-06-19 (v3 sync sprint). Verbatim.
use super::{BackfillResp, P2PSyncState, sane_raise};
use crate::block_store::{BlockStore, StoredBlock};
use sigil_header::{BlockHash, SigilBlockHeaderV0};
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
    // STAGE A path (behavior unchanged): decode inline, then ONE batched store write.
    // The decode is now factored into `decode_backfill_headers` so LANE-C's write-back
    // ring (STAGE B) can commit off the verify hot path via `decode_verify_backfill`.
    let headers = decode_backfill_headers(bytes);
    store.put_blocks_batch(&headers)
}

/// DECODE half of the backfill path — bytes → headers, NO store write. Factored out of
/// `ingest_backfill_bytes` so the LANE-C write-back ring can commit off the verify hot
/// path (`mod.rs`: `let h = verify::decode_verify_backfill(&b); commit_buf.push(&mut store, &h)`).
/// Handles all three wire codecs: `'Z'` zstd(bincode) (codec=1, 14× less wire), `'H'`
/// raw bincode, and legacy full-block `BackfillResp` JSON. Returns an EMPTY vec on any
/// unparseable / oversized / bad-bincode body (logged, never panics, never zstd-bombs —
/// see the 64 MiB cap in `zstd_decompress_body`); the caller treats that exactly like a
/// benched peer. Byte-for-byte the same decode the inline path used before the split.
pub(super) fn decode_backfill_headers(bytes: &[u8]) -> Vec<SigilBlockHeaderV0> {
    match bytes.first() {
        // v0.33: 'Z' = zstd-compressed headers. Decompress (capped) → bincode Vec<Header>.
        Some(&b'Z') => match zstd_decompress_body(&bytes[1..]) {
            Some(body) => bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body).unwrap_or_else(|_| {
                crate::tlog!("[p2p-sync] resp: bad zstd header bincode ({} B)", bytes.len());
                Vec::new()
            }),
            None => {
                crate::tlog!("[p2p-sync] resp: zstd decompress failed ({} B)", bytes.len());
                Vec::new()
            }
        },
        // v0.10.0: 'H' = raw bincode Vec<Header> (one batched write at the call site).
        Some(&b'H') => bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..]).unwrap_or_else(|_| {
            crate::tlog!("[p2p-sync] resp: bad header bincode ({} B)", bytes.len());
            Vec::new()
        }),
        // legacy full-block JSON envelope.
        _ => {
            if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
                resp.blocks
                    .iter()
                    .filter_map(|v| v.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()))
                    .collect()
            } else {
                crate::tlog!("[p2p-sync] resp: unparseable ({} bytes)", bytes.len());
                Vec::new()
            }
        }
    }
}

/// DECODE + per-header VERIFY for LANE-C STAGE B's write-back ring. Returns only the
/// WELL-FORMED headers in the chunk, `precheck()`'d in parallel across cores — the
/// per-header, order-INDEPENDENT half of the verify stage (charter scope #2 at the
/// chunk level), moved off the sync-loop thread. The order-DEPENDENT parent-linkage is
/// still established by the store + the `chain_verify::verify_to_parallel` spine walk
/// after commit (32-byte stored-hash compare), so dropping a precheck-failing header
/// here only ever removes an invalid block early — the store would reject it anyway.
/// Seam: `mod.rs` calls this, then `commit_buf.push(&mut store, &headers)`.
pub(super) fn decode_verify_backfill(bytes: &[u8]) -> Vec<SigilBlockHeaderV0> {
    use rayon::prelude::*;
    decode_backfill_headers(bytes)
        .into_par_iter()
        .filter(|h| h.precheck().is_ok())
        .collect()
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

// ============================================================================
// LANE-B PRIORITY 1 — checkpoint fast-path for 0->tip catch-up (v3 sync sprint)
// ============================================================================
//
// THE MULTIPLIER toward the 92.6k stretch: do NOT full-verify every one of ~128M
// headers. Bulk-TRUST the deep prefix below a genesis-anchored TRUSTED checkpoint
// and full-verify ONLY the frontier window beneath the anchor (+ everything above
// it, via the existing periodic loop). Per-block precheck+linkage then runs on
// ~`frontier_window` blocks instead of the whole chain.
//
// SOUNDNESS — read before touching (verified by DeepSeek, adversarial review):
//   * The trust comes from the EXTERNAL ANCHOR, not from any fold proof. A
//     `flux_fold` proof over the prefix is PoK-of-openings over a PEER-SUPPLIED
//     commitment vector + a FLAT random-linear combination — it does NOT prove the
//     prefix is a valid parent-linked chain of REAL headers, and being a sum it is
//     order-independent so it cannot capture linkage. The fold ALONE is therefore
//     unsound for advancing the watermark. We gate bulk-trust on a genesis-anchored
//     trusted checkpoint hash (source: sigil-dns-anchor SQIsign-L5 tip, or a
//     validator quorum) — the Bitcoin-style checkpoint model. For a tip-following
//     light client the pre-checkpoint prefix is irrelevant to CURRENT state; we
//     only need the checkpoint authentic + the frontier fully verified.
//   * The frontier walk PINS the window to the anchor: authenticate the stored
//     block at the anchor height (its ingest hash == the trusted hash), then
//     full-verify [floor, anchor]. Parent-linkage cascades the anchor's
//     authenticity DOWN to `floor` — a forged bulk-trusted prefix that fed a wrong
//     floor cannot both link to the authentic anchor's parent_hash chain and be
//     wrong. Anything above the anchor links UP off the authenticated block.
//   * fail-loud: a wrong/missing anchor, a corrupt stored hash, or a frontier that
//     breaks (non-Missing) before reaching the anchor => Err, the watermark is left
//     where it was (never advanced over un-linked data), and the caller falls back
//     to full `verify_to_parallel`. "State divergence is impossible to hide."
//
// A `flux_fold` ATTESTATION of the served prefix commitment vector (tamper-evidence
// + bandwidth saving when paired with LANE-A codec=2 skeleton elision — NOT
// authorization) is a deferred follow-on: it needs the flux-fold dep on sigil-top +
// a producer-side header-witness prover, and adds nothing to the soundness above.
// Tracked with rocky-sync-lead.

/// Blocks BELOW the trusted anchor we STILL full-verify (belt-and-suspenders even
/// under a trusted checkpoint). The charter's "frontier (tip-50k)".
pub(super) const DEFAULT_FRONTIER_WINDOW: u64 = 50_000;

/// Why a checkpoint fast-forward refused. EVERY variant means the verified watermark
/// was LEFT WHERE IT WAS — the caller must fall back to full verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FastForwardError {
    /// The trusted anchor's block isn't stored yet — can't authenticate it.
    AnchorBlockMissing { height: u64 },
    /// The stored block at the anchor height does NOT hash to the trusted anchor —
    /// a peer served a different chain. Hard reject.
    AnchorMismatch { height: u64, expected: String, found: String },
    /// The full-verify of the frontier broke (precheck/linkage) BELOW the anchor —
    /// the downloaded frontier does not link up to the authenticated checkpoint.
    FrontierBreak { height: u64, reason: String },
    /// The stored anchor block's `hash_hex` is not valid hex (store corruption).
    CorruptStoredHash { height: u64 },
}

impl std::fmt::Display for FastForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FastForwardError::AnchorBlockMissing { height } =>
                write!(f, "anchor block h={height} not stored — cannot authenticate checkpoint"),
            FastForwardError::AnchorMismatch { height, expected, found } =>
                write!(f, "anchor MISMATCH at h={height}: trusted={expected} but stored block hashes to {found} (peer served a different chain)"),
            FastForwardError::FrontierBreak { height, reason } =>
                write!(f, "frontier does not link to the trusted anchor — break at h={height}: {reason}"),
            FastForwardError::CorruptStoredHash { height } =>
                write!(f, "stored hash_hex at anchor h={height} is corrupt hex"),
        }
    }
}

/// Outcome of a successful checkpoint fast-forward.
#[derive(Debug, Clone)]
pub(super) struct FastForwardReport {
    /// Height of the trusted (externally-anchored) checkpoint.
    pub trusted_height: u64,
    /// The verified-watermark FLOOR we set: the prefix `[base, bulk_trusted_below)`
    /// is bulk-trusted (authenticated transitively by the frontier's downward link
    /// cascade to the anchor); never below the previous watermark (monotonic).
    pub bulk_trusted_below: u64,
    /// The verified watermark AFTER the bounded frontier walk.
    pub verified_to: u64,
    /// Headers the frontier walk actually full-verified this pass.
    pub frontier_checked: u64,
}

/// Decode a stored 64-hex block hash. `None` on malformed hex (caller surfaces it as
/// store corruption, never a silent pass).
fn decode_stored_hash(h: &str) -> Option<BlockHash> {
    hex::decode(h).ok()?.try_into().ok()
}

/// LANE-B P1: fast-forward the verified watermark to a genesis-anchored TRUSTED
/// checkpoint, bulk-trusting the deep prefix and full-verifying only the frontier.
///
/// `anchor_height` / `trusted_anchor_hash` come from a trust root OUTSIDE this node
/// (sigil-dns-anchor SQIsign-L5 tip, or a validator quorum) — the fold proof does
/// NOT authorize them (see the soundness note above). `frontier_window` blocks below
/// the anchor are STILL fully verified.
///
/// Returns the watermark movement on success, or fails LOUD (watermark untouched
/// past what actually linked) so the caller falls back to full verification.
pub(super) fn fast_forward_to_anchored_checkpoint(
    store: &mut BlockStore,
    anchor_height: u64,
    trusted_anchor_hash: &BlockHash,
    frontier_window: u64,
) -> Result<FastForwardReport, FastForwardError> {
    // (1) AUTHENTICATE the anchor. The stored `hash_hex` was computed by our own
    //     ingest over the stored bytes, so equality with the trusted hash proves we
    //     hold EXACTLY the anchored block.
    let stored = store
        .get_stored_at_height(anchor_height)
        .ok_or(FastForwardError::AnchorBlockMissing { height: anchor_height })?;
    let got = decode_stored_hash(&stored.hash_hex)
        .ok_or(FastForwardError::CorruptStoredHash { height: anchor_height })?;
    if got != *trusted_anchor_hash {
        return Err(FastForwardError::AnchorMismatch {
            height: anchor_height,
            expected: hex::encode(trusted_anchor_hash),
            found: hex::encode(got),
        });
    }

    // (2) BULK-TRUST the deep prefix: fast-FORWARD the watermark to the floor
    //     (anchor - frontier_window), clamped to base. Monotonic — NEVER lower it
    //     (max-wins). The [floor, anchor] band below is still fully re-verified in (3).
    let base = store.base();
    let floor = anchor_height.saturating_sub(frontier_window).max(base);
    if floor > store.verified_to() {
        store.set_verified_to(floor);
    }
    let bulk_trusted_below = store.verified_to();

    // (3) FULL-VERIFY the frontier window up to (and past) the anchor. Budget covers
    //     floor->anchor + the window so one pass is guaranteed to reach the anchor;
    //     the periodic loop continues anchor->tip afterwards. verify_to_parallel does
    //     precheck + 32-byte parent linkage across cores.
    let budget = anchor_height
        .saturating_sub(bulk_trusted_below)
        .saturating_add(frontier_window)
        .max(1);
    let report = crate::chain_verify::verify_to_parallel(store, budget);

    // (4) PIN to the anchor. The walk must reach PAST the anchor cleanly; reaching
    //     past it means [floor, anchor] linked contiguously and anchor+1 linked off
    //     the authenticated anchor block. A non-Missing break at/below the anchor =>
    //     the downloaded frontier does not link to the trusted checkpoint -> refuse.
    //     A `Missing` break (download frontier not fully arrived) is NOT corruption:
    //     the watermark advanced contiguously + linked as far as the data exists; a
    //     later tick + re-call finishes the pin.
    if report.verified_to <= anchor_height {
        if let Some((h, reason)) = &report.first_break {
            if *reason != crate::chain_verify::BreakReason::Missing {
                return Err(FastForwardError::FrontierBreak {
                    height: *h,
                    reason: reason.to_string(),
                });
            }
        }
    }

    Ok(FastForwardReport {
        trusted_height: anchor_height,
        bulk_trusted_below,
        verified_to: report.verified_to,
        frontier_checked: report.checked,
    })
}

/// 6.0.6 root-cause snapshot fast-forward. `pull_snapshot` already verified the skeleton
/// (contiguity + parent linkage + recomputed root == trailer) AND bound `snap_anchor_hash` to
/// the chain's true last record (fetch.rs finalize). This authenticates that verified anchor
/// against the trust root (dns_anchor_tip's SQIsign/dev anchor) and, on match, BULK-TRUSTS the
/// skeleton prefix by advancing the verified watermark to the floor (anchor - frontier_window).
/// Bodies are NOT required here -- they backfill via PASS-2; the snapshot is skeleton-only, which
/// is exactly why the main-store path of fast_forward_to_anchored_checkpoint could never authenticate it.
pub(super) fn fast_forward_from_authenticated_snapshot(
    store: &mut BlockStore,
    snap_anchor_height: u64,
    snap_anchor_hash: &BlockHash,
    trusted_height: u64,
    trusted_hash: &BlockHash,
    frontier_window: u64,
) -> Result<u64, FastForwardError> {
    if snap_anchor_height != trusted_height || snap_anchor_hash != trusted_hash {
        return Err(FastForwardError::AnchorMismatch {
            height: trusted_height,
            expected: hex::encode(trusted_hash),
            found: hex::encode(snap_anchor_hash),
        });
    }
    // Record the authenticated trusted checkpoint FIRST: set_verified_to clamps to
    // max(synced_to, fold_anchor_height()). In a skeleton-only sync synced_to==0 (no bodies
    // downloaded), so the fold anchor is what lets the verified watermark advance over the
    // trusted prefix; bodies then backfill against this checkpoint via PASS-2.
    store.set_fold_anchor(trusted_height, *trusted_hash);
    let floor = trusted_height.saturating_sub(frontier_window).max(store.base());
    if floor > store.verified_to() {
        store.set_verified_to(floor);
    }
    Ok(store.verified_to())
}

#[cfg(test)]
mod fast_path_tests {
    use super::*;
    use crate::block_store::BlockStore;
    use sigil_header::*;

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-ffwd-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    /// Valid, internally-consistent, correctly-linked header (mirrors chain_verify's
    /// builder so the happy path prechecks: vdf_input == BLAKE3(parent || nonce)).
    fn mk_header(height: u64, parent_hash: BlockHash) -> SigilBlockHeaderV0 {
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
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
            sig_scheme: scheme,
            producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
            topology_commitment: None,
        }
    }

    fn mk_chain(n: u64) -> Vec<SigilBlockHeaderV0> {
        let mut chain = Vec::new();
        let mut parent = [0u8; 32];
        for h in 0..n {
            let hdr = mk_header(h, parent);
            parent = hdr.hash();
            chain.push(hdr);
        }
        chain
    }

    /// Happy path: a correct trusted anchor fast-forwards the watermark to the floor
    /// and the frontier walk verifies up past the anchor (pinning it).
    #[test]
    fn anchored_checkpoint_fast_forwards_and_pins() {
        const N: u64 = 200;
        let win = 50u64;
        let anchor = 150u64;
        let chain = mk_chain(N);
        let p = tmp("happy");
        let _ = std::fs::remove_dir_all(&p);
        let mut s = BlockStore::open(&p).unwrap();
        s.put_blocks_batch(&chain);
        s.advance();
        assert_eq!(s.verified_to(), 0, "nothing verified before the fast-path");

        let anchor_hash = chain[anchor as usize].hash();
        let rep = fast_forward_to_anchored_checkpoint(&mut s, anchor, &anchor_hash, win)
            .expect("correct anchor must fast-forward");

        assert_eq!(rep.bulk_trusted_below, anchor - win, "watermark floor = anchor - window");
        // The frontier walk reached past the anchor → the window is pinned to it.
        assert!(rep.verified_to > anchor, "frontier verified up past the anchor: {rep:?}");
        assert_eq!(rep.verified_to, N, "clean chain verifies the whole frontier to tip");
        assert!(s.verified_to() >= anchor - win, "watermark persisted at >= floor");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// A WRONG trusted anchor is rejected fail-loud and the watermark is NOT advanced.
    #[test]
    fn wrong_anchor_is_rejected_and_watermark_untouched() {
        const N: u64 = 100;
        let chain = mk_chain(N);
        let p = tmp("wrong-anchor");
        let _ = std::fs::remove_dir_all(&p);
        let mut s = BlockStore::open(&p).unwrap();
        s.put_blocks_batch(&chain);
        s.advance();
        let before = s.verified_to();

        let bogus = [0xEEu8; 32]; // not the hash of block 80
        let err = fast_forward_to_anchored_checkpoint(&mut s, 80, &bogus, 50)
            .expect_err("a wrong anchor MUST be rejected");
        assert!(matches!(err, FastForwardError::AnchorMismatch { height: 80, .. }), "got {err:?}");
        assert_eq!(s.verified_to(), before, "watermark must NOT advance on a bad anchor");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// The anchor block not being stored yet => fail-loud (cannot authenticate).
    #[test]
    fn missing_anchor_block_is_rejected() {
        const N: u64 = 50;
        let chain = mk_chain(N);
        let p = tmp("missing-anchor");
        let _ = std::fs::remove_dir_all(&p);
        let mut s = BlockStore::open(&p).unwrap();
        s.put_blocks_batch(&chain);
        s.advance();
        // anchor at a height we never stored.
        let anchor_hash = [1u8; 32];
        let err = fast_forward_to_anchored_checkpoint(&mut s, 9_999, &anchor_hash, 50)
            .expect_err("missing anchor block must be rejected");
        assert!(matches!(err, FastForwardError::AnchorBlockMissing { height: 9_999 }), "got {err:?}");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn authenticated_snapshot_advances_watermark_and_rejects_forgery() {
        let p = tmp("auth-snap");
        let _ = std::fs::remove_dir_all(&p);
        let mut s = BlockStore::open(&p).unwrap();
        let anchor = 150u64; let win = 50u64; let anchor_hash = [7u8; 32];
        // matching snapshot anchor == trust root -> bulk-trust to floor (anchor - win)
        let vt = fast_forward_from_authenticated_snapshot(&mut s, anchor, &anchor_hash, anchor, &anchor_hash, win)
            .expect("matching anchor authenticates");
        assert_eq!(vt, anchor - win, "verified watermark advanced to the bulk-trust floor");
        // forged snapshot anchor (hash != trust root) -> reject, watermark unchanged
        let forged = [9u8; 32];
        let err = fast_forward_from_authenticated_snapshot(&mut s, anchor, &forged, anchor, &anchor_hash, win)
            .expect_err("a snapshot anchor != trust root must be rejected");
        assert!(matches!(err, FastForwardError::AnchorMismatch { .. }), "got {err:?}");
        assert_eq!(s.verified_to(), anchor - win, "watermark not changed by a rejected forgery");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// Defense-in-depth: a corrupt block INSIDE the frontier window (below the anchor)
    /// breaks the linkage cascade to the authenticated anchor → refuse, fail-loud.
    #[test]
    fn forged_frontier_below_anchor_breaks_loud() {
        const N: u64 = 200;
        let win = 50u64;
        let anchor = 150u64;
        let mut chain = mk_chain(N);
        // Re-forge block 120 (inside [anchor-win, anchor) = [100,150)) to a wrong but
        // internally-consistent parent so precheck passes but linkage to 119 breaks.
        chain[120] = mk_header(120, [0xAB; 32]);
        let p = tmp("forged-frontier");
        let _ = std::fs::remove_dir_all(&p);
        let mut s = BlockStore::open(&p).unwrap();
        // The forged 120 won't link to 119 via the strict batch path; force it in so we
        // exercise the fast-path's own defense (mirrors chain_verify's force_insert test).
        for hdr in &chain[..120] { let _ = s.put_block_fast(hdr.clone()); }
        s.force_insert_block(chain[120].clone());
        for hdr in &chain[121..] { let _ = s.put_block_fast(hdr.clone()); }
        s.advance();

        let anchor_hash = chain[anchor as usize].hash();
        let err = fast_forward_to_anchored_checkpoint(&mut s, anchor, &anchor_hash, win)
            .expect_err("a forged frontier that doesn't link to the anchor MUST be rejected");
        assert!(matches!(err, FastForwardError::FrontierBreak { height: 120, .. }), "got {err:?}");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// LANE-C STAGE-B enabler: the decode split round-trips all wire codecs and the
    /// precheck filter drops malformed headers before commit. Garbage → empty, no panic.
    #[test]
    fn decode_backfill_roundtrips_and_precheck_filters() {
        let chain = mk_chain(8);
        let body = bincode::serialize(&chain).unwrap();

        // 'H' raw-bincode codec.
        let mut h_frame = vec![b'H'];
        h_frame.extend_from_slice(&body);
        let got_h = decode_backfill_headers(&h_frame);
        assert_eq!(got_h.len(), 8);
        assert_eq!(got_h, chain, "'H' decode is byte-exact");

        // 'Z' zstd(bincode) codec — C encoder, our pure-Rust decode path.
        let mut z_frame = vec![b'Z'];
        z_frame.extend_from_slice(&zstd::encode_all(&body[..], 1).unwrap());
        let got_z = decode_backfill_headers(&z_frame);
        assert_eq!(got_z, chain, "'Z' decode round-trips to the same headers");

        // decode_verify_backfill keeps all well-formed headers...
        assert_eq!(decode_verify_backfill(&z_frame).len(), 8);

        // ...and DROPS one whose vdf_input binding is broken (precheck fail).
        let mut bad = chain.clone();
        bad[2].vdf_input = [0u8; 32]; // breaks vdf_input == BLAKE3(parent || nonce)
        let mut z_bad = vec![b'Z'];
        z_bad.extend_from_slice(&zstd::encode_all(&bincode::serialize(&bad).unwrap()[..], 1).unwrap());
        assert_eq!(decode_verify_backfill(&z_bad).len(), 7, "precheck-failing header is filtered out");

        // unparseable → empty, never panics.
        assert!(decode_backfill_headers(b"not a frame at all").is_empty());
    }

    /// LANE-B verify-side test for the SkeletonStore (lead #497): the flat store must
    /// preserve the LINKAGE CHAIN that the bulk-trusted prefix relies on — i.e. every
    /// stored record's block_hash == the real header.hash() and parent_hash ==
    /// prev.block_hash, through a full append→read_at round-trip. The store's own tests
    /// use arbitrary (unlinked) records; this proves REAL linked skeletons survive intact,
    /// so the prefix→frontier linkage cascade (which pins to the trusted anchor) reads
    /// uncorrupted block_hash/parent_hash off the flat store.
    #[test]
    fn skeleton_store_preserves_linkage_chain() {
        use crate::block_sync::skeleton_store::SkeletonStore;
        use sigil_header::SkeletonRecord;

        let base = 1u64;
        let n = 500u64; // heights base..=base+n-1
        let chain = mk_chain(base + n); // linked headers, heights 0..base+n-1
        let recs: Vec<SkeletonRecord> = chain[base as usize..(base + n) as usize]
            .iter()
            .map(SkeletonRecord::from_header)
            .collect();
        assert_eq!(recs.len() as u64, n);

        let p = tmp("skel-linkage");
        let _ = std::fs::remove_file(&p);
        let mut s = SkeletonStore::open(&p, base).expect("open skeleton store");
        assert_eq!(s.append(&recs).expect("append linked run"), n as usize);
        assert_eq!(s.tip_height(), Some(base + n - 1));
        assert_eq!(s.count(), n);

        // Read back every record + assert the trust-relevant invariants survive the store.
        let mut prev_hash = chain[(base - 1) as usize].hash(); // parent of the first stored height
        for h in base..base + n {
            let r = s.read_at(h).expect("read_at ok").expect("record present");
            assert_eq!(r.height, h);
            assert_eq!(r.block_hash, chain[h as usize].hash(), "block_hash == real header.hash()");
            assert_eq!(r.parent_hash, prev_hash, "linkage chain preserved through the flat store");
            prev_hash = r.block_hash;
        }
        assert!(s.read_at(base + n).expect("read_at ok").is_none(), "past-tip read is None");
        let _ = std::fs::remove_file(&p);
    }

    /// PROOF for the 92.6k verify-hash optimization (A's d35a57b, lead #543/#544): hashing
    /// a SkeletonRecord's raw fields (height_le ‖ block_hash ‖ parent_hash) is BYTE-IDENTICAL
    /// to hashing `bincode::serialize(rec)` — so dropping the per-record bincode re-encode in
    /// SnapshotVerifier leaves `archive_root` UNCHANGED. This locks that invariant: if anyone
    /// ever changes SkeletonRecord's layout/serde so the two diverge, this fails loud BEFORE a
    /// silent archive_root mismatch can break verification.
    #[test]
    fn archive_root_field_hash_equals_bincode_per_record() {
        use sigil_header::SkeletonRecord;
        // Varied record set (heights + distinct 32-byte hashes).
        let recs: Vec<SkeletonRecord> = (0..1000u64)
            .map(|i| {
                let mut bh = [0u8; 32];
                bh[0] = i as u8;
                bh[1] = (i >> 8) as u8;
                bh[31] = 0xAB;
                let mut ph = [0u8; 32];
                ph[0] = i.wrapping_sub(1) as u8;
                ph[7] = 0xCD;
                SkeletonRecord { height: i, block_hash: bh, parent_hash: ph }
            })
            .collect();

        // The invariant the equivalence rests on: a record is exactly 72 B under bincode,
        // default LE fixint, no framing → bincode(rec) == height_le8 ‖ block_hash32 ‖ parent_hash32.
        assert_eq!(bincode::serialize(&recs[0]).unwrap().len(), 72);

        // OLD path: archive_root = BLAKE3 over bincode::serialize(rec) per record.
        let mut old = blake3::Hasher::new();
        for r in &recs {
            old.update(&bincode::serialize(r).unwrap());
        }
        let old_root = *old.finalize().as_bytes();

        // NEW path (A's d35a57b): BLAKE3 over the raw fields, no per-record bincode alloc.
        let mut new = blake3::Hasher::new();
        for r in &recs {
            new.update(&r.height.to_le_bytes());
            new.update(&r.block_hash);
            new.update(&r.parent_hash);
        }
        let new_root = *new.finalize().as_bytes();

        assert_eq!(
            old_root, new_root,
            "field-hash archive_root MUST equal bincode-per-record — the verify-hash opt must not change the root"
        );
    }
}
