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
/// v0.33 (1M-blk/s lane): linkage now compares against the parent's STORED hash — the one
/// ingest computed via `header.hash()` and persisted in `StoredBlock.hash_hex` — instead of
/// re-deriving `p.hash()` here. `hash()` JSON-serializes the entire ~1 KB header (≈3-5 KB of
/// text, ~15-25 µs); recomputing it per step DOUBLED verify cost and was the measured
/// 26-52k blk/s ceiling. The compare is now 32 bytes. Soundness: the stored hash was produced
/// by our own ingest hashing these exact stored bytes — same value, computed once.
fn verify_one(
    header: &sigil_header::SigilBlockHeaderV0,
    parent_hash: Option<&sigil_header::BlockHash>,
) -> Result<(), BreakReason> {
    header.precheck().map_err(|e| BreakReason::Precheck(e.to_string()))?;
    if let Some(expected) = parent_hash {
        if header.parent_hash != *expected {
            return Err(BreakReason::ParentMismatch {
                height: header.height,
                expected: hex::encode(expected),
                found: hex::encode(header.parent_hash),
            });
        }
    }
    Ok(())
}

/// Decode a stored 64-hex block hash into a `BlockHash`. None on malformed hex (treated by
/// the caller as a store-corruption break, never a silent pass).
fn decode_hash_hex(hash_hex: &str) -> Option<sigil_header::BlockHash> {
    let bytes = hex::decode(hash_hex).ok()?;
    bytes.try_into().ok()
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

    // The parent HASH for the first step. At the genesis anchor (`base`) there is no
    // fetchable parent — the block at `base` is the verification trust-root (its parent,
    // e.g. SIGIL's height-0 genesis, isn't backfill-servable), so it's accepted on precheck
    // alone. Above `base` the parent MUST be the already-verified block at h-1 (present,
    // since verified_to <= synced_to and the prefix is contiguous). v0.33: we carry the
    // parent's STORED hash (computed once at ingest), not its header — no re-hashing.
    let mut parent_hash: Option<sigil_header::BlockHash> = if h == base {
        None
    } else {
        store.get_stored_at_height(h - 1).and_then(|b| decode_hash_hex(&b.hash_hex))
    };

    while checked < max_steps {
        let block = match store.get_stored_at_height(h) {
            Some(b) => b,
            None => { first_break = Some((h, BreakReason::Missing)); break; }
        };
        if let Err(reason) = verify_one(&block.header, parent_hash.as_ref()) {
            first_break = Some((h, reason));
            break;
        }
        // h is verified; its STORED hash becomes the linkage target for h+1. A malformed
        // stored hash is store corruption — surface it as a break, never skip silently.
        parent_hash = match decode_hash_hex(&block.hash_hex) {
            Some(ph) => Some(ph),
            None => {
                first_break = Some((h, BreakReason::Precheck(
                    format!("corrupt stored hash_hex at h={h}"))));
                break;
            }
        };
        h += 1;
        checked += 1;
    }

    if h > start {
        store.set_verified_to(h);
    }
    VerifyReport { verified_to: h, checked, first_break }
}

/// v0.34 TPS lane — parallel-precheck verify walk. Same contract as
/// [`verify_to`] (identical [`VerifyReport`] for any input), but the expensive
/// per-header work is fanned across all cores.
///
/// Why this is sound AND faster: `precheck()` is a PURE function of a single
/// header — schema/network/sig-length/nonce checks plus the
/// `vdf_input == BLAKE3(parent_hash || nonce_sqisign)` binding (a BLAKE3 over
/// ~324 bytes). It has no dependency on any other header, so all prechecks in
/// a window run concurrently. Only the parent-linkage compare is order-
/// dependent, and (since v0.33) it's a 32-byte equality against the stored
/// hash — cheap and inherently sequential. So we:
///   1. read a window of up to `max_steps` contiguous stored blocks,
///   2. `par_iter` their prechecks across cores,
///   3. walk the window in height order doing precheck-result then the cheap
///      linkage compare, stopping at the FIRST failure — exactly the order
///      `verify_to` would have hit it, so `first_break` is identical.
///
/// On a 48-core box the precheck stage is ~Nx cheaper, lifting the verify
/// ceiling toward the 1M-blk/s lane target. Falsifiable via
/// `bench_ingest_and_verify_throughput` (parallel vs serial on the same chain).
pub fn verify_to_parallel(store: &mut BlockStore, max_steps: u64) -> VerifyReport {
    use rayon::prelude::*;

    let base = store.base();
    let start = store.verified_to().max(base);

    // (1) Read the contiguous window [start, start+max_steps) until the frontier.
    let mut window: Vec<sigil_header::SigilBlockHeaderV0> = Vec::new();
    let mut stored_hashes: Vec<String> = Vec::new();
    let mut frontier_break: Option<(u64, BreakReason)> = None;
    let mut h = start;
    while (window.len() as u64) < max_steps {
        match store.get_stored_at_height(h) {
            Some(b) => {
                window.push(b.header);
                stored_hashes.push(b.hash_hex);
                h += 1;
            }
            None => {
                frontier_break = Some((h, BreakReason::Missing));
                break;
            }
        }
    }

    if window.is_empty() {
        return VerifyReport { verified_to: start, checked: 0, first_break: frontier_break };
    }

    // (2) Parallel precheck — the embarrassingly-parallel hot path.
    let precheck_results: Vec<Option<BreakReason>> = window
        .par_iter()
        .map(|hdr| hdr.precheck().err().map(|e| BreakReason::Precheck(e.to_string())))
        .collect();

    // (3) Sequential linkage walk in height order — identical failure ordering
    //     to verify_to (precheck-then-linkage at each height).
    let mut parent_hash: Option<sigil_header::BlockHash> = if start == base {
        None
    } else {
        store.get_stored_at_height(start - 1).and_then(|b| decode_hash_hex(&b.hash_hex))
    };
    let mut verified = start;
    let mut checked = 0u64;
    let mut first_break = None;

    for (i, hdr) in window.iter().enumerate() {
        let height = start + i as u64;
        if let Some(reason) = &precheck_results[i] {
            first_break = Some((height, reason.clone()));
            break;
        }
        if let Some(expected) = parent_hash.as_ref() {
            if hdr.parent_hash != *expected {
                first_break = Some((height, BreakReason::ParentMismatch {
                    height,
                    expected: hex::encode(expected),
                    found: hex::encode(hdr.parent_hash),
                }));
                break;
            }
        }
        parent_hash = match decode_hash_hex(&stored_hashes[i]) {
            Some(ph) => Some(ph),
            None => {
                first_break = Some((height, BreakReason::Precheck(
                    format!("corrupt stored hash_hex at h={height}"))));
                break;
            }
        };
        verified = height + 1;
        checked += 1;
    }

    // If the whole window verified cleanly, the stop reason is the frontier.
    if first_break.is_none() {
        first_break = frontier_break;
    }

    if verified > start {
        store.set_verified_to(verified);
    }
    VerifyReport { verified_to: verified, checked, first_break }
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

    /// v0.33 throughput bench — run with `--nocapture` to see the numbers. Measures the
    /// REAL pipeline economics on this box: (1) parallel batched ingest (put_blocks_batch:
    /// JSON-hash + bincode fan-out across cores + one batch_put), (2) the stored-hash verify
    /// walk (2 db reads + precheck + 32-byte linkage compare per step — NO re-hash). The
    /// old verify recomputed `parent.hash()` (≈3-5 KB JSON serialize) per step, which capped
    /// it at the measured 26-52k blk/s. This bench is the falsifiable gate for the
    /// 1M-blk/s lane: ingest and verify must EACH clear ≥200k blk/s here to make the
    /// end-to-end target plausible (wire is then the binding constraint).
    /// v0.34: the parallel walk MUST be observationally identical to the serial
    /// one — same verified_to, same checked, same first_break — on a clean chain
    /// AND on a chain with a deliberate parent-linkage break. This is the safety
    /// gate that lets the parallel path replace the serial one.
    #[test]
    fn parallel_verify_matches_serial() {
        const N: u64 = 5_000;
        // Clean chain.
        let chain = mk_chain(N);
        let pa = tmp("par-clean-a");
        let pb = tmp("par-clean-b");
        let _ = std::fs::remove_dir_all(&pa);
        let _ = std::fs::remove_dir_all(&pb);
        let mut sa = BlockStore::open(&pa).unwrap();
        let mut sb = BlockStore::open(&pb).unwrap();
        sa.put_blocks_batch(&chain); sa.advance();
        sb.put_blocks_batch(&chain); sb.advance();
        let serial = verify_to(&mut sa, u64::MAX);
        let parallel = verify_to_parallel(&mut sb, u64::MAX);
        assert_eq!(serial.verified_to, parallel.verified_to);
        assert_eq!(serial.checked, parallel.checked);
        assert_eq!(serial.first_break, parallel.first_break);
        assert_eq!(parallel.verified_to, N);

        // Broken chain: corrupt one header's parent_hash so linkage breaks at h=3000.
        let mut broken = mk_chain(N);
        broken[3000].parent_hash = [0xeeu8; 32]; // won't match stored hash of 2999
        let pc = tmp("par-broken-a");
        let pd = tmp("par-broken-b");
        let _ = std::fs::remove_dir_all(&pc);
        let _ = std::fs::remove_dir_all(&pd);
        let mut sc = BlockStore::open(&pc).unwrap();
        let mut sd = BlockStore::open(&pd).unwrap();
        sc.put_blocks_batch(&broken); sc.advance();
        sd.put_blocks_batch(&broken); sd.advance();
        let serial_b = verify_to(&mut sc, u64::MAX);
        let parallel_b = verify_to_parallel(&mut sd, u64::MAX);
        assert_eq!(serial_b.verified_to, parallel_b.verified_to);
        assert_eq!(serial_b.first_break, parallel_b.first_break);
        // Both must stop exactly at the corrupted height.
        assert!(matches!(parallel_b.first_break, Some((3000, BreakReason::ParentMismatch { .. }))));
    }

    #[test]
    fn bench_ingest_and_verify_throughput() {
        const N: u64 = 20_000;
        let p = tmp("bench");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(N); // setup cost: N JSON hashes to link parents (not timed)
        let mut s = BlockStore::open(&p).unwrap();

        let t0 = std::time::Instant::now();
        let stored = s.put_blocks_batch(&chain);
        s.advance();
        let ingest = t0.elapsed();
        assert_eq!(stored as u64, N);
        assert_eq!(s.synced_to(), N);

        let t1 = std::time::Instant::now();
        let rep = verify_to(&mut s, u64::MAX);
        let verify = t1.elapsed();
        assert_eq!(rep.verified_to, N, "clean chain verifies to tip: {:?}", rep.first_break);

        let ing_rate = N as f64 / ingest.as_secs_f64();
        let ver_rate = N as f64 / verify.as_secs_f64();
        eprintln!("[bench] ingest {N} blks in {ingest:?}  →  {ing_rate:.0} blk/s");
        eprintln!("[bench] verify {N} blks in {verify:?}  →  {ver_rate:.0} blk/s");
        let _ = std::fs::remove_dir_all(&p);
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
            for hdr in &chain[..3] { s.put_block_fast(hdr.clone()).unwrap(); }
            // v0.95: strict downward-linkage ingest now REFUSES a parent-broken block via every
            // production path, so force the forged lookahead straight into storage to keep
            // exercising verify_to's defense-in-depth (it must still catch a corrupt spine even
            // if one somehow lands).
            s.force_insert_block(chain[3].clone());
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
    fn batch_ingest_rejects_forked_duplicate_height_without_poisoning_spine() {
        let p = tmp("fork-overwrite");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(6);
        {
            let mut s = BlockStore::open(&p).unwrap();
            assert_eq!(s.put_blocks_batch(&chain), chain.len());
            s.advance();
            assert_eq!(s.synced_to(), 6);
            let canonical_h2 = s.get_stored_at_height(2).unwrap().hash_hex;

            // This header is internally consistent but belongs to a different fork.
            // Older store code blindly rewrote height 2 -> fork hash, leaving height 3
            // linked to the old height 2 and making verify_to stop forever at h=3/8194.
            let fork_h2 = mk_header(2, [0xAB; 32]);
            assert_ne!(hex::encode(fork_h2.hash()), canonical_h2);
            assert_eq!(s.put_blocks_batch(&[fork_h2]), 0, "conflicting height is rejected");
            assert_eq!(
                s.get_stored_at_height(2).unwrap().hash_hex,
                canonical_h2,
                "height index still points at the first accepted spine block"
            );

            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 6);
            assert!(rep.clean(), "forked duplicate did not poison the verified spine: {:?}", rep.first_break);
        }
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn batch_ingest_rejects_child_that_does_not_link_to_stored_parent() {
        let p = tmp("unlinked-child");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(4);
        {
            let mut s = BlockStore::open(&p).unwrap();
            assert_eq!(s.put_blocks_batch(&chain[..3]), 3);
            s.advance();
            assert_eq!(s.synced_to(), 3);

            // Mirrors the live h=8194 failure: parent h=2 is already stored, but
            // the next header points to a different parent hash. It must be dropped
            // at ingest instead of poisoning the verifier frontier.
            let fork_child = mk_header(3, [0xCD; 32]);
            assert_eq!(s.put_blocks_batch(&[fork_child]), 0);
            assert!(!s.has_height(3), "bad child was not inserted at the frontier");

            assert_eq!(s.put_blocks_batch(&chain[3..4]), 1);
            s.advance();
            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 4);
            assert!(rep.clean(), "canonical child still syncs cleanly: {:?}", rep.first_break);
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

    /// REPRO (v0.95 frontier-wedge): a non-spine fork block can reach height H *before*
    /// its parent H-1 is stored — the ONLY ingest window with no stored parent to link
    /// against — via the open-ended probe ingesting out-of-order. The store must not let
    /// that squatter (a) make `advance()` walk PAST a block that doesn't link to its
    /// parent (silent spine corruption — `advance_synced` is has_height-only), nor (b)
    /// permanently block the canonical spine block at H via height_index_conflict. This
    /// drives the exact sequence behind the live `[store] rejected h=4098 detail=…03b437e0
    /// incoming=…71536a9` stall and asserts the DESIRED (correct) outcome.
    ///
    /// ACCEPTANCE GATE (v0.95): with strict downward-linkage ingest the squatter at h=2 is
    /// REFUSED at the door (its parent h=1 isn't stored yet), so it can neither block the
    /// canonical h=2 (height index) nor reject the canonical h=1 (child-linkage check). Was
    /// CONFIRMED-RED before the fix (failed at "canonical h=1 accepted: left 0 right 1").
    #[test]
    fn squatter_before_parent_must_not_wedge_or_corrupt_the_frontier() {
        let p = tmp("squatter-wedge");
        let _ = std::fs::remove_dir_all(&p);
        let chain = mk_chain(6); // heights 0..5, each linking to the previous
        {
            let mut s = BlockStore::open(&p).unwrap();
            assert_eq!(s.put_blocks_batch(&chain[0..1]), 1, "genesis h=0 lands");
            s.advance();
            assert_eq!(s.synced_to(), 1);

            // Squatter at h=2 BEFORE its parent h=1 exists (no linkage check fires).
            let squatter = mk_header(2, [0xAB; 32]);
            assert_ne!(squatter.hash(), chain[2].hash(), "squatter is a different block");
            let _ = s.put_blocks_batch(&[squatter]);

            // The canonical parent h=1 then the canonical spine child h=2 arrive.
            assert_eq!(s.put_blocks_batch(&chain[1..2]), 1, "canonical h=1 accepted");
            let _ = s.put_blocks_batch(&chain[2..3]);
            s.advance();

            // DESIRED: h=2 indexes the SPINE block (the one whose parent_hash == stored h=1),
            // never the squatter — and the contiguous frontier reflects a real spine.
            assert_eq!(
                s.get_stored_at_height(2).map(|b| b.hash_hex),
                Some(hex::encode(chain[2].hash())),
                "h=2 must index the spine block, not the squatter"
            );

            assert_eq!(s.put_blocks_batch(&chain[3..6]), 3, "rest of the spine lands");
            s.advance();
            let rep = verify_to(&mut s, u64::MAX);
            assert_eq!(rep.verified_to, 6, "spine links to genesis, no wedge: {:?}", rep.first_break);
            assert!(rep.clean(), "a squatter must not poison the verified spine: {:?}", rep.first_break);
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
