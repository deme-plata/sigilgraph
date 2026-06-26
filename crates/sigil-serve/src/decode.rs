//! Parallel client-side transport decode — the **LANE-3 codec-wall collapse**.
//!
//! The v7 end-to-end wall is not serve-encode (4.09M blk/s) nor commit (SST-ingest
//! 251–300k) nor verify (4.98M) — it is the CLIENT receive path: inflate + bincode
//! deserialize of block-pack pages, measured at **50,499 blk/s** (sequential
//! pure-Rust ruzstd + bincode) in `commit_pipeline_bench`'s `[transport]` row. At
//! ~46k sustained end-to-end this single stage caps the whole pipeline below 100k.
//!
//! The fix is page-level data parallelism: each transport page (≤ `CHUNK`=4096
//! headers, so a real multi-million-block sync is thousands of pages) is an
//! INDEPENDENT inflate+deserialize unit. [`decode_zstd_header_pages_parallel`]
//! runs them across a rayon pool and re-assembles in global order. On the 48-core
//! build host this turns the 50,499 blk/s serial decode into an N-page-parallel
//! decode that clears the ≥100k target with wide margin (page count is the only
//! ceiling — and it scales with sync size).
//!
//! Pure-Rust ruzstd is kept (the Windows cross-build stays mingw-clean — same
//! decoder the follower already runs in verify.rs). zstd-DICTIONARY inflate was
//! evaluated and DEFERRED: ruzstd 0.7 exposes no dictionary API, and a trained
//! dict gives little on SIGIL headers (the bulk is high-entropy STARK/SQIsign/VDF
//! proof bytes, not repetitive framing) — not worth a ruzstd bump + a producer
//! wire change for a 2× target the rayon path already beats. See the swarm thread.
//!
//! LIBRARY only: verify.rs (rocky-sync-B) adopts these via a thin call-site seam
//! at the batch-decode site — this module never touches verify.rs.

use rayon::prelude::*;
use sigil_header::{SigilBlockHeaderV0, SkeletonRecord};

/// Inflated-size cap — the zstd-bomb guard. Matches verify.rs `zstd_decompress_body`
/// (responder 4096-item cap × ~16 KiB/header headroom). A mature SIGIL header is
/// ~8 KB (STARK + VDF + 2×SQIsign + fluxc bundle), so a 4096-header page can be
/// ~32 MB raw; 64 MiB leaves headroom while still bailing on a bomb.
pub const DEFAULT_MAX_INFLATE: u64 = 64 * 1024 * 1024;

/// Pure-Rust ruzstd inflate of one zstd frame, capped at `max_out` (the streaming
/// decoder bails at `max_out + 1`, never allocating past it). `None` on a malformed
/// frame or a would-be-oversize inflate — the caller drops/benches the page exactly
/// as today, never panics. Byte-for-byte the decoder verify.rs runs in prod.
pub fn inflate_zstd(frame: &[u8], max_out: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut dec = ruzstd::StreamingDecoder::new(frame).ok()?;
    let mut out = Vec::new();
    dec.take(max_out + 1).read_to_end(&mut out).ok()?;
    if out.len() as u64 > max_out {
        return None; // zstd-bomb guard
    }
    Some(out)
}

/// Decode ONE tagless zstd page (the responder's `'Z'` body, tag already stripped):
/// inflate then `bincode::deserialize::<Vec<header>>`. `None` on any failure.
pub fn decode_zstd_header_page(frame: &[u8], max_out: u64) -> Option<Vec<SigilBlockHeaderV0>> {
    let inflated = inflate_zstd(frame, max_out)?;
    bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&inflated).ok()
}

/// Decode ONE tagged transport page as the prod wire frames it:
/// `'Z'` + zstd(bincode) | `'H'` + bincode | (legacy full-block JSON is handled by
/// verify.rs's existing path, not here). `None` on unknown tag / failure. This is the
/// per-page primitive the verify.rs seam calls; the parallel driver is below.
pub fn decode_tagged_header_page(page: &[u8], max_out: u64) -> Option<Vec<SigilBlockHeaderV0>> {
    match page.first()? {
        b'Z' => decode_zstd_header_page(&page[1..], max_out),
        b'H' => bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&page[1..]).ok(),
        _ => None,
    }
}

/// Re-assemble a per-page Vec-of-Vecs into one ordered Vec. `rayon`'s indexed
/// `collect()` preserves input order, so concatenation is globally ordered.
#[inline]
fn flatten_ordered(per_page: Vec<Vec<SigilBlockHeaderV0>>) -> Vec<SigilBlockHeaderV0> {
    let total: usize = per_page.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for mut page in per_page {
        out.append(&mut page);
    }
    out
}

/// **THE FIX** — decode many TAGLESS zstd pages in parallel, returning all headers in
/// global order. rayon `par_iter` inflates + deserializes each independent page across
/// the pool; a page that fails to decode contributes nothing (the caller benches the
/// peer — never a panic, never a whole-batch stall). Use this at the batch site where
/// multiple block-packs are in hand; for a single page use [`decode_zstd_header_page`].
pub fn decode_zstd_header_pages_parallel<B>(pages: &[B], max_out: u64) -> Vec<SigilBlockHeaderV0>
where
    B: AsRef<[u8]> + Sync,
{
    let per_page: Vec<Vec<SigilBlockHeaderV0>> = pages
        .par_iter()
        .map(|p| decode_zstd_header_page(p.as_ref(), max_out).unwrap_or_default())
        .collect();
    flatten_ordered(per_page)
}

/// Parallel decode of TAGGED (`'Z'`/`'H'`) pages — the verify.rs drop-in for a batch
/// of mixed-codec block-packs. Order-preserving; failed pages drop to empty.
pub fn decode_tagged_header_pages_parallel<B>(pages: &[B], max_out: u64) -> Vec<SigilBlockHeaderV0>
where
    B: AsRef<[u8]> + Sync,
{
    let per_page: Vec<Vec<SigilBlockHeaderV0>> = pages
        .par_iter()
        .map(|p| decode_tagged_header_page(p.as_ref(), max_out).unwrap_or_default())
        .collect();
    flatten_ordered(per_page)
}

// ── skeleton ('S') pages: already inflate-free, but parallelize the stride parse ──

/// Parse one tagless `'S'` skeleton page body: `bincode(Vec<SkeletonRecord>)` =
/// `u64 count` ‖ `count × 72 B`. Zero re-encode — reads the fixed strides directly
/// (matches the client `fetch.rs` parse). `None` on a malformed/short body.
pub fn decode_skeleton_page(body: &[u8]) -> Option<Vec<SkeletonRecord>> {
    if body.len() < 8 {
        return None;
    }
    let count = u64::from_le_bytes(body[0..8].try_into().ok()?) as usize;
    let recs = &body[8..];
    if recs.len() != count.checked_mul(72)? {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for chunk in recs.chunks_exact(72) {
        let height = u64::from_le_bytes(chunk[0..8].try_into().ok()?);
        let mut block_hash = [0u8; 32];
        block_hash.copy_from_slice(&chunk[8..40]);
        let mut parent_hash = [0u8; 32];
        parent_hash.copy_from_slice(&chunk[40..72]);
        out.push(SkeletonRecord { height, block_hash, parent_hash });
    }
    Some(out)
}

/// Parallel decode of many tagless `'S'` skeleton page bodies, in global order.
pub fn decode_skeleton_pages_parallel<B>(pages: &[B]) -> Vec<SkeletonRecord>
where
    B: AsRef<[u8]> + Sync,
{
    let per_page: Vec<Vec<SkeletonRecord>> = pages
        .par_iter()
        .map(|p| decode_skeleton_page(p.as_ref()).unwrap_or_default())
        .collect();
    let total: usize = per_page.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for mut page in per_page {
        out.append(&mut page);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::*;

    /// Minimal but real header (precheck not needed here — we test decode fidelity).
    /// `pad` models the heavy proof payload in vdf_proof.pi.
    fn mk_header(height: u64, parent_hash: BlockHash, pad: usize) -> SigilBlockHeaderV0 {
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
            vdf_proof: WesolowskiProof { y: vec![], pi: vec![0xABu8; pad], t: 100 },
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
        }
    }

    fn mk_chain(n: u64, pad: usize) -> Vec<SigilBlockHeaderV0> {
        let mut chain = Vec::with_capacity(n as usize);
        let mut parent = [0u8; 32];
        for h in 0..n {
            let hdr = mk_header(h, parent, pad);
            parent = hdr.hash();
            chain.push(hdr);
        }
        chain
    }

    /// Page a chain into CHUNK-sized 'Z' blobs (tagless zstd of bincode(Vec<header>)),
    /// exactly as the responder emits and commit_pipeline_bench measures.
    fn zstd_pages(chain: &[SigilBlockHeaderV0], chunk: usize) -> Vec<Vec<u8>> {
        chain
            .chunks(chunk)
            .map(|c| {
                let raw = bincode::serialize(&c.to_vec()).expect("ser");
                zstd::encode_all(&raw[..], 1).expect("zstd")
            })
            .collect()
    }

    fn sequential_decode(pages: &[Vec<u8>]) -> Vec<SigilBlockHeaderV0> {
        let mut out = Vec::new();
        for z in pages {
            let inf = inflate_zstd(z, DEFAULT_MAX_INFLATE).expect("inflate");
            let mut v: Vec<SigilBlockHeaderV0> = bincode::deserialize(&inf).expect("de");
            out.append(&mut v);
        }
        out
    }

    #[test]
    fn parallel_equals_sequential_and_preserves_order() {
        let chain = mk_chain(9_000, 512); // 3 pages at CHUNK=4096 (4096+4096+808)
        let pages = zstd_pages(&chain, 4096);
        assert!(pages.len() >= 2, "need multiple pages to test parallelism");

        let seq = sequential_decode(&pages);
        let par = decode_zstd_header_pages_parallel(&pages, DEFAULT_MAX_INFLATE);

        assert_eq!(par.len(), chain.len());
        assert_eq!(seq, par, "parallel decode must equal sequential");
        // global order: heights are 0..n contiguous
        for (i, h) in par.iter().enumerate() {
            assert_eq!(h.height, i as u64, "order broken at {i}");
        }
        assert_eq!(par, chain, "decoded chain must equal the original");
    }

    #[test]
    fn tagged_pages_roundtrip() {
        let chain = mk_chain(5_000, 256);
        // build 'Z'-tagged pages
        let z: Vec<Vec<u8>> = zstd_pages(&chain, 4096)
            .into_iter()
            .map(|body| {
                let mut o = vec![b'Z'];
                o.extend(body);
                o
            })
            .collect();
        let par = decode_tagged_header_pages_parallel(&z, DEFAULT_MAX_INFLATE);
        assert_eq!(par, chain);

        // a single 'H' (raw bincode) page also decodes
        let raw = bincode::serialize(&chain[0..10].to_vec()).unwrap();
        let mut h_page = vec![b'H'];
        h_page.extend(raw);
        assert_eq!(decode_tagged_header_page(&h_page, DEFAULT_MAX_INFLATE).unwrap().len(), 10);
    }

    #[test]
    fn bad_page_drops_to_empty_not_panic() {
        let garbage = vec![vec![0xFFu8; 32], vec![0x00u8; 8]];
        let out = decode_zstd_header_pages_parallel(&garbage, DEFAULT_MAX_INFLATE);
        assert!(out.is_empty(), "malformed pages contribute nothing, no panic");
        assert!(inflate_zstd(&[0xFF; 16], DEFAULT_MAX_INFLATE).is_none());
    }

    #[test]
    fn inflate_bomb_guard_trips() {
        // ~1 MiB of zeros compresses tiny but inflates large; cap at 4 KiB → None.
        let raw = vec![0u8; 1024 * 1024];
        let comp = zstd::encode_all(&raw[..], 1).unwrap();
        assert!(inflate_zstd(&comp, 4096).is_none(), "must trip the bomb guard");
        assert!(inflate_zstd(&comp, DEFAULT_MAX_INFLATE).is_some());
    }

    #[test]
    fn skeleton_page_parallel_roundtrip() {
        // build 'S' bodies = u64 count ‖ count×72B, two pages
        let mk_body = |base: u64, n: u64| -> Vec<u8> {
            let mut b = (n).to_le_bytes().to_vec();
            for i in 0..n {
                b.extend((base + i).to_le_bytes());
                b.extend([(base + i) as u8; 32]);
                b.extend([(base + i) as u8 ^ 0xFF; 32]);
            }
            b
        };
        let pages = vec![mk_body(0, 1000), mk_body(1000, 1000)];
        let recs = decode_skeleton_pages_parallel(&pages);
        assert_eq!(recs.len(), 2000);
        assert_eq!(recs[0].height, 0);
        assert_eq!(recs[1999].height, 1999);
    }
}
