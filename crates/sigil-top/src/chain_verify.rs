// sigil-top/src/chain_verify.rs — Full verifying sync (v0.9.0)
//
// THE 0.9.0 feature. sigil-top no longer merely *downloads* block headers genesis→tip
// and trusts them — it now *verifies the whole chain as a single connected spine* and
// records a `verified_to` watermark that is cryptographically meaningful:
//
//   blocks 0..verified_to have each
//     1. passed `SigilBlockHeaderV0::precheck()` — schema/version/network_id, signature
//        LENGTH, nonce well-formedness, AND the internal-consistency invariant
//        `vdf_input == BLAKE3(parent_hash || nonce_sqisign)`; and
//     2. linked to their parent: `header[h].parent_hash == header[h-1].hash()`.
//
// This is SIGIL claim #2 ("state divergence is impossible to hide") made operational on
// the light client: a peer cannot feed us a bag of unrelated-but-individually-plausible
// headers and have us call it "synced" — the spine has to actually connect, all the way
// down to genesis, or `verified_to` stalls at the first break and we say so loudly.
//
// HONEST scope (what this does NOT yet check):
//   • The SQIsign producer signature and Wesolowski VDF proof are NOT cryptographically
//     verified here — those need flux-sqisign / flux-vdf verify entrypoints wired in
//     (gated behind the `sqisign` feature, follow-on). `precheck()` checks their SHAPE
//     and the VDF-input binding, not the underlying hardness. So `verified_to` proves
//     "connected, well-formed, internally-consistent chain", not "every proof re-checked".
//   • The 4 state roots / STARK transition proof are committed in the header (and so are
//     covered by the parent-linkage hash chain) but not independently re-derived — that
//     needs full block bodies + the state machine (Phase 3, flux-zk-stark gate).
//
// What it DOES give, today, end-to-end and testable: an unforgeable answer to "is the
// chain I downloaded one real chain back to genesis?".

use crate::block_store::BlockStore;

/// Why the verified spine stopped advancing at a given height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakReason {
    /// The header for this height isn't stored yet (we've verified everything that's
    /// contiguously present; this is the normal "caught up to the download frontier"
    /// terminator, not corruption).
    Missing,
    /// `precheck()` rejected the header (schema/network/sig-length/nonce/vdf-input).
    Precheck(String),
    /// `header[h].parent_hash != header[h-1].hash()` — the spine does not connect. This
    /// is the load-bearing check: a real corruption / fork / forged-header break.
    ParentMismatch { height: u64, expected: String, found: String },
}

impl std::fmt::Display for BreakReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakReason::Missing => write!(f, "missing (download frontier)"),
            BreakReason::Precheck(e) => write!(f, "precheck failed: {e}"),
            BreakReason::ParentMismatch { height, expected, found } =>
                write!(f, "parent linkage broken at h={height}: header.parent_hash={found} but hash(block[{}])={expected}", height - 1),
        }
    }
}

/// Result of a verification walk.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// New contiguous verified watermark (blocks 0..verified_to are validated).
    pub verified_to: u64,
    /// How many headers this walk actually checked (excludes the already-verified prefix).
    pub checked: u64,
    /// Why the walk stopped, and at what height. `Missing` = caught up to the downloaded
    /// frontier (clean); anything else = a genuine integrity break that needs attention.
    pub first_break: Option<(u64, BreakReason)>,
}

impl VerifyReport {
    /// True when the walk stopped only because it hit the download frontier (no corruption).
    pub fn clean(&self) -> bool {
        matches!(self.first_break, None | Some((_, BreakReason::Missing)))
    }
}

/// Verify one header against its predecessor's hash. Genesis (h==0) has no parent to
/// link to, so it only needs to precheck. Returns Ok(()) or the reason it failed.
fn verify_one(
    header: &sigil_header::SigilBlockHeaderV0,
    parent: Option<&sigil_header::SigilBlockHeaderV0>,
) -> Result<(), BreakReason> {
    header.precheck().map_err(|e| BreakReason::Precheck(e.to_string()))?;
    if let Some(p) = parent {
        let expected = p.hash();
        if header.parent_hash != expected {
            return Err(BreakReason::ParentMismatch {
                height: header.height,
                expected: hex::encode(expected),
                found: hex::encode(header.parent_hash),
            });
        }
    }
    Ok(())
}

/// Advance the store's `verified_to` watermark by walking forward from the current
/// watermark, validating each consecutive header (precheck + parent linkage) until it
/// hits a break or runs `max_steps` headers. Persists the new watermark via the store.
///
/// `max_steps` bounds the work done in one call so the sync loop stays responsive on a
/// multi-million-block chain — pass `u64::MAX` (or a big number) for an exhaustive walk
/// (the `verify-chain` subcommand), a few thousand for an incremental tick.
pub fn verify_to(store: &mut BlockStore, max_steps: u64) -> VerifyReport {
    let base = store.base();
    let start = store.verified_to().max(base);
    let mut h = start;
    let mut checked = 0u64;
    let mut first_break = None;

    // The parent header for the first step. At the genesis anchor (`base`) there is no
    // fetchable parent — the block at `base` is the verification trust-root (its parent,
    // e.g. SIGIL's height-0 genesis, isn't backfill-servable), so it's accepted on precheck
    // alone. Above `base` the parent MUST be the already-verified block at h-1 (present,
    // since verified_to <= synced_to and the prefix is contiguous).
    let mut parent = if h == base { None } else { store.get_header_at_height(h - 1) };

    while checked < max_steps {
        let header = match store.get_header_at_height(h) {
            Some(hd) => hd,
            None => { first_break = Some((h, BreakReason::Missing)); break; }
        };
        if let Err(reason) = verify_one(&header, parent.as_ref()) {
            first_break = Some((h, reason));
            break;
        }
        // h is verified; it becomes the parent of h+1.
        parent = Some(header);
        h += 1;
        checked += 1;
    }

    if h > start {
        store.set_verified_to(h);
    }
    VerifyReport { verified_to: h, checked, first_break }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::BlockStore;
    use sigil_header::*;

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-cverify-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    /// Build a valid, internally-consistent, correctly-linked header at `height` whose
    /// parent is `parent_hash`. We mirror exactly what `precheck()` demands so the
    /// happy-path chain verifies: well-formed nonce + vdf_input == BLAKE3(parent||nonce).
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
        }
    }

    /// Build a correctly-linked chain of `n` headers (heights 0..n), each parent_hash =
    /// hash of the previous. Returns them in height order.
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

    #[test]
    fn clean_chain_verifies_to_tip_and_persists() {
        let p = tmp("clean");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(6);
        {
            let mut s = BlockStore::open(&p).unwrap();
            for hdr in &chain { s.put_block_fast(hdr.clone()).unwrap(); }
            s.advance();
            assert_eq!(s.synced_to(), 6);
            assert_eq!(s.verified_to(), 0, "nothing verified before the walk");

            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 6, "all 6 link cleanly back to genesis");
            assert_eq!(rep.checked, 6);
            assert!(rep.clean(), "stopped only at the frontier: {:?}", rep.first_break);
            assert!(matches!(rep.first_break, Some((6, BreakReason::Missing))));
            assert_eq!(s.verified_to(), 6, "watermark persisted in-memory");
        }
        // Re-open: verification watermark RESUMES from disk, doesn't re-walk from 0.
        let s2 = BlockStore::open(&p).unwrap();
        assert_eq!(s2.verified_to(), 6, "verified_to survived restart");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn parent_break_stops_the_spine_at_the_break() {
        let p = tmp("break");
        let _ = std::fs::remove_dir_all(&p);
        let mut chain = mk_chain(5);
        // Re-forge block 3 to point at a WRONG but internally-consistent parent. mk_header
        // recomputes vdf_input from the bogus parent, so `precheck()` PASSES — this isolates
        // the parent-LINKAGE check. (A raw `chain[3].parent_hash = …` edit would instead be
        // caught earlier by precheck's `vdf_input == BLAKE3(parent_hash‖nonce)` binding — a
        // nice belt-and-suspenders property, but not what THIS test is exercising.)
        chain[3] = mk_header(3, [0xAB; 32]);
        {
            let mut s = BlockStore::open(&p).unwrap();
            for hdr in &chain { s.put_block_fast(hdr.clone()).unwrap(); }
            s.advance();
            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 3, "0,1,2 verify; 3 breaks the spine");
            assert!(!rep.clean(), "a parent break is NOT clean");
            match rep.first_break {
                Some((3, BreakReason::ParentMismatch { height, .. })) => assert_eq!(height, 3),
                other => panic!("expected ParentMismatch at 3, got {other:?}"),
            }
            assert_eq!(s.verified_to(), 3);
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn precheck_failure_is_caught() {
        let p = tmp("precheck");
        let _ = std::fs::remove_dir_all(&p);
        let mut chain = mk_chain(4);
        // Break block 2's vdf_input invariant (precheck must reject it). Parent linkage of
        // block 2 is still correct, so this isolates the precheck path.
        chain[2].vdf_input = [0x00; 32];
        // Re-link block 3 to the (now precheck-failing but still hashable) block 2 so the
        // ONLY reason the walk stops at 2 is precheck, not a downstream parent mismatch.
        chain[3].parent_hash = chain[2].hash();
        {
            let mut s = BlockStore::open(&p).unwrap();
            for hdr in &chain { s.put_block_fast(hdr.clone()).unwrap(); }
            s.advance();
            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 2, "0,1 verify; 2 fails precheck");
            assert!(matches!(rep.first_break, Some((2, BreakReason::Precheck(_)))));
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn max_steps_bounds_work_and_resumes() {
        let p = tmp("bounded");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(10);
        {
            let mut s = BlockStore::open(&p).unwrap();
            for hdr in &chain { s.put_block_fast(hdr.clone()).unwrap(); }
            s.advance();
            // First tick: only 4 steps.
            let r1 = verify_to(&mut s, 4);
            assert_eq!(r1.verified_to, 4);
            assert_eq!(r1.checked, 4);
            // Second tick resumes from 4, not 0.
            let r2 = verify_to(&mut s, 4);
            assert_eq!(r2.verified_to, 8);
            assert_eq!(r2.checked, 4);
            // Final tick reaches the tip and stops clean at the frontier.
            let r3 = verify_to(&mut s, u64::MAX);
            assert_eq!(r3.verified_to, 10);
            assert!(r3.clean());
        }
        let _ = std::fs::remove_dir_all(&p);
    }
}
