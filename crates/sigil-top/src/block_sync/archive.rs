//! block_sync/archive.rs — PASS 2: background full-archive body backfill.
//!
//! Viktor's model: the skeleton is the FIRST pass (fast, verified chain structure via
//! `flux_db::skeleton::SkeletonStore` + its `archive_root` anchor). This is the SECOND
//! pass — it walks that skeleton and fills the `BlockStore` with the full block BODIES,
//! converging EVERY node to a full archive.
//!
//! Trustless by construction: pass 1 already committed the BLAKE3 `block_hash` for every
//! height. So a fetched body is accepted only if `header.hash() == skeleton.block_hash`.
//! A lying peer can serve any bytes it likes — they're dropped unless they hash to the
//! skeleton's commitment. The archive can never be poisoned; the skeleton is the truth.
//!
//! Runs at LOW priority off the frontier sync. The frontier (pass 1) keeps the node tip-
//! current in seconds; pass 2 backfills the deep history underneath it over time.

use crate::block_sync::skel_flux::{FluxSkeletonStore, SkelRec};
use crate::block_store::BlockStore;
use sigil_header::SigilBlockHeaderV0;

/// The next contiguous run of heights that have a SKELETON record (so we know their
/// committed hash) but no BODY in the store yet — the work pass 2 requests from peers.
/// Bounded to `chunk` so a single fetch stays a normal backfill request. `None` when the
/// archive is complete up to the skeleton tip (every node is now a full archive).
pub(super) fn next_body_gap(
    skel: &FluxSkeletonStore,
    store: &BlockStore,
    base: u64,
    chunk: u64,
) -> Option<(u64, u64)> {
    let tip = skel.tip_seq()?; // highest height the skeleton covers
    let mut from = None;
    let mut h = base;
    while h <= tip {
        if !store.has_height(h) {
            from = Some(h);
            break;
        }
        h += 1;
    }
    let from = from?;
    let mut to = from;
    while to < tip && (to - from) + 1 < chunk.max(1) && !store.has_height(to + 1) {
        to += 1;
    }
    Some((from, to))
}

/// Verify fetched bodies against the skeleton's committed hashes and store the matches.
/// Returns (stored, rejected). A body with no skeleton entry or a hash mismatch is
/// DROPPED — never stored — so the archive stays exactly the chain the skeleton attests.
pub(super) fn ingest_bodies_verified(
    skel: &mut FluxSkeletonStore,
    store: &mut BlockStore,
    bodies: &[SigilBlockHeaderV0],
) -> (usize, usize) {
    let mut good: Vec<SigilBlockHeaderV0> = Vec::with_capacity(bodies.len());
    let mut rejected = 0usize;
    for b in bodies {
        match skel.read_at(b.height) {
            Ok(Some(SkelRec(rec))) if rec.block_hash == b.hash() => good.push(b.clone()),
            _ => rejected += 1, // missing skeleton entry OR hash != commitment → reject
        }
    }
    let stored = if good.is_empty() { 0 } else { store.put_blocks_batch(&good) };
    (stored, rejected)
}

/// Archive completion 0.0..=1.0 over [base, skeleton_tip] — drives a "full-archive: N%"
/// UI readout so the operator watches the node converge to a full archive.
pub(super) fn archive_fraction(skel: &FluxSkeletonStore, store: &BlockStore, base: u64) -> f64 {
    let Some(tip) = skel.tip_seq() else { return 0.0 };
    if tip < base {
        return 1.0;
    }
    let span = (tip - base + 1) as f64;
    let mut have = 0u64;
    let mut h = base;
    while h <= tip {
        if store.has_height(h) {
            have += 1;
        }
        h += 1;
    }
    (have as f64 / span).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::*;

    fn mk(height: u64, parent: BlockHash) -> SigilBlockHeaderV0 {
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        let mut hh = blake3::Hasher::new();
        hh.update(&parent);
        hh.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
        let scheme = SigScheme::SqiSign5;
        SigilBlockHeaderV0 {
            version: HEADER_VERSION, network_id: NETWORK_ID, height, parent_hash: parent,
            merge_parents: Vec::new(), timestamp_ms: 1000 + height, nonce_sqisign: nonce,
            vdf_input, vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 }, difficulty: 1,
            wallet_state_root: [0u8; 32], dex_state_root: [0u8; 32], event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32], tx_count: 0,
            fluxc_artifact_proof: ProofBundle { artifact_blake3: [0u8; 32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None },
            sig_scheme: scheme, producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
        }
    }

    fn chain(n: u64) -> Vec<SigilBlockHeaderV0> {
        let mut out = Vec::new();
        let mut parent = [0u8; 32];
        for h in 0..=n {
            let hdr = mk(h, parent);
            parent = hdr.hash();
            if h >= 1 { out.push(hdr); }
        }
        out
    }

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-archive-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    /// A body that matches the skeleton commitment is stored; a TAMPERED body (right
    /// height, wrong contents → wrong hash) is rejected. The trustless guarantee.
    #[test]
    fn pass2_stores_matching_bodies_and_rejects_tampered_ones() {
        let sp = tmp("skel");
        let bp = tmp("store");
        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);

        let blocks = chain(20);
        // Pass 1: skeleton holds the committed hash for every height.
        let mut skel: FluxSkeletonStore = flux_db::skeleton::SkeletonStore::open(&sp, 1).unwrap();
        let recs: Vec<SkelRec> = blocks.iter().map(|h| SkelRec(SkeletonRecord::from_header(h))).collect();
        skel.append(&recs).unwrap();

        let mut store = BlockStore::open_blocking(&bp).unwrap();
        store.set_base(1);

        // Honest bodies → all stored.
        let (stored, rejected) = ingest_bodies_verified(&mut skel, &mut store, &blocks);
        assert_eq!(stored, 20, "every body matching the skeleton hash is stored");
        assert_eq!(rejected, 0);

        // Tampered body: same height, different contents → different hash → rejected.
        let mut liar = mk(5, [0xAB; 32]); // wrong parent → wrong hash
        liar.height = 5;
        let mut store2 = BlockStore::open_blocking(&tmp("store2")).unwrap();
        store2.set_base(1);
        let (s2, r2) = ingest_bodies_verified(&mut skel, &mut store2, &[liar]);
        assert_eq!(s2, 0, "a body whose hash != the skeleton commitment is NOT stored");
        assert_eq!(r2, 1, "and is counted as rejected");

        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
        let _ = std::fs::remove_dir_all(&tmp("store2"));
    }

    #[test]
    fn next_gap_and_fraction_track_archive_convergence() {
        let sp = tmp("gskel");
        let bp = tmp("gstore");
        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
        let blocks = chain(10);
        let mut skel: FluxSkeletonStore = flux_db::skeleton::SkeletonStore::open(&sp, 1).unwrap();
        skel.append(&blocks.iter().map(|h| SkelRec(SkeletonRecord::from_header(h))).collect::<Vec<_>>()).unwrap();
        let mut store = BlockStore::open_blocking(&bp).unwrap();
        store.set_base(1);
        // Nothing stored yet → gap starts at base, archive 0%.
        assert_eq!(next_body_gap(&skel, &store, 1, 4096), Some((1, 10)));
        assert!(archive_fraction(&skel, &store, 1) < 0.01);
        // Fill them → no gap, archive 100%.
        ingest_bodies_verified(&mut skel, &mut store, &blocks);
        assert_eq!(next_body_gap(&skel, &store, 1, 4096), None, "archive complete → no gap");
        assert!((archive_fraction(&skel, &store, 1) - 1.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
    }
}
