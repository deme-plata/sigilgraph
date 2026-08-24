//! `wedge_writer_gap` — an honest coverage note, plus what IS testable here.
//!
//! ## UPDATE 2026-08-24: the gap below is CLOSED
//!
//! `crates/sigil-top/src/block_store.rs` was extracted to the `sigil-block-store`
//! library crate (same move `sigil_sync::SyncStore` made for the range-lifecycle
//! half of sync, described below when this module could not yet make it). This
//! crate now depends on `sigil-block-store` directly and drives the REAL
//! `BlockStore::put_block` / `put_blocks_batch` / `put_blocks_bulk_trusted` —
//! see [`wedge_conflicts_stay_local_at_scale`] below, which is the scenario this
//! whole module was written to eventually make possible. The historical
//! "cannot cover this" analysis is kept below verbatim because it's still the
//! correct record of WHY the gap existed and what closing it required — read it
//! for that reasoning, not as a currently-true statement of what's untestable.
//!
//! ## What this did NOT cover, and why (historical — now closed, see above)
//!
//! The actual "SPINE BREAK — STUCK" wedge fixed today (commit `b43a107`,
//! shipped as sigil-top v7.1.76) lives in `crates/sigil-top/src/block_store.rs`:
//! a height-keyed on-disk index with three write paths, one of which
//! (`put_blocks_bulk_trusted`) used to write unconditionally with no conflict
//! check, silently poisoning a height that the other two (checked) paths then
//! refused to ever correct.
//!
//! `sigil-chronos` could not drive that real code, for the SAME reason
//! `sync.rs`'s own module docs already record for a different subsystem:
//! `sigil-top` is a **binary-only** crate (see its `Cargo.toml` — `[[bin]]`,
//! no `[lib]`), so nothing outside it can `use sigil_top::block_store`.
//! `sigil_sync::SyncStore` was extracted to a library crate precisely so
//! `sync.rs` could stop re-modelling a copy and drive the real logic instead
//! — the identical move would fix this gap, but `block_store.rs` is 1,951
//! lines of disk-backed (`flux_db`-based) state, not a small pure-logic unit,
//! so that extraction was a real, separate piece of work, not a quick add-on
//! to this scenario. It has now been done — see `crates/sigil-block-store/`.
//!
//! Separately, and just as relevant: [`crate::SigilSimNode::apply_block`] is
//! a STRICTLY SEQUENTIAL state machine (`block.header.height !=
//! self.next_height` is an instant reject) — it has no height-keyed
//! random-access cache at all. The block_store bug is fundamentally about
//! out-of-order / conflicting writes to a height-indexed map; this sim
//! model's shape cannot represent that class of fault even in principle,
//! independent of the binary-only-crate problem. A faithful port would need
//! a genuinely different sim node shape, not just an import fix.
//!
//! ## What IS testable here, and is: a related question at the STATE layer
//!
//! The heal-marker mitigation (`heal_wedged_store_once`, v7.1.75) fires
//! **once per exact version string** and then never again under that marker
//! — which is exactly what let the real bug re-poison an already-healed
//! store. That raises a fair question one layer up, in code THIS crate does
//! drive for real: does [`SigilSimNode`]'s own safety net —
//! [`ApplyOutcome::Divergence`], the actual exit-78 root-mismatch check —
//! have the same one-shot weakness? i.e. after it fires once, does detection
//! stay live for a SECOND, independent fault, or does something about
//! catching the first one leave the node less able to catch a second?
//!
//! [`divergence_detection_has_no_one_shot_blind_spot`] answers this directly
//! against the real [`commit_state_transition`] chokepoint: inject a fault,
//! confirm it's caught, then inject a SECOND, unrelated fault and confirm
//! it is caught too, with the count correctly at 2. This is not the same bug
//! as the block_store wedge — it is the same SHAPE of question
//! ("does a safety mechanism exhaust itself after firing once?") asked of a
//! layer this crate can actually exercise honestly today.

use flux_chronos::NodeId;
use sigil_header::SigilBlockHeaderV0;

use crate::{demo_genesis, sign_dummy, ApplyOutcome, Block, SigilSimNode};

/// Build a well-formed next block from `producer`'s own state, then corrupt
/// one committed root so a follower applying it must detect divergence.
/// Mirrors the shape of a real disagreement between two independently
/// computed views of the same height — the general fault class the block
/// store bug and this scenario both sit inside, one layer apart.
/// `demo_genesis()` only funds wallets `[1;32]..=[5;32]` — every send in this
/// module must stay within that range or the producer's balance check
/// refuses the tx and `produce_one` returns `None` with nothing to mint.
fn funded_wallet(i: u8) -> [u8; 32] {
    [(i % 5) + 1; 32]
}

fn mint_then_corrupt_a_root(producer: &mut SigilSimNode, seed: u8) -> Block {
    let from = funded_wallet(seed);
    let to = funded_wallet(seed + 1);
    producer.enqueue_tx(sign_dummy(sigil_tx::SigilTx::Send {
        from,
        to,
        amount: 1,
        token: sigil_tx::NATIVE,
        fee: 0,
    }));
    let mut block = producer.produce_one().expect("producer mints");
    tamper_wallet_root(&mut block.header);
    block
}

fn tamper_wallet_root(header: &mut SigilBlockHeaderV0) {
    header.wallet_state_root[0] ^= 0xFF;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE GATE. A safety net that only catches the FIRST fault and goes
    /// quiet for the second would be exactly as dangerous, in spirit, as a
    /// heal marker that only fires once per version — the operator would
    /// reasonably believe protection is still live when it is not. Proven
    /// against the real chokepoint: two independent, unrelated faults, both
    /// must be caught, both must be counted.
    #[test]
    fn divergence_detection_has_no_one_shot_blind_spot() {
        let g = demo_genesis();
        let mut producer = SigilSimNode::new("producer", NodeId(0), vec![], true, 1_000_000, &g);
        let mut follower = SigilSimNode::new("follower", NodeId(1), vec![], false, 1_000_000, &g);

        // A few honest blocks first — the common case must stay clean.
        for i in 0..3u8 {
            producer.enqueue_tx(sign_dummy(sigil_tx::SigilTx::Send {
                from: funded_wallet(i), to: funded_wallet(i + 1),
                amount: 1, token: sigil_tx::NATIVE, fee: 0,
            }));
            let b = producer.produce_one().expect("producer mints");
            assert_eq!(follower.apply_external_block(&b), ApplyOutcome::Ok);
        }
        assert_eq!(follower.divergence_count, 0, "honest prefix must not false-positive");

        // FAULT #1 — must be caught. `apply_block` returns before committing
        // on a root mismatch, so the follower's height/state are UNCHANGED
        // by this call — it is still sitting at the same height afterward.
        let height_before = follower.height();
        let bad1 = mint_then_corrupt_a_root(&mut producer, 1);
        assert_eq!(
            follower.apply_external_block(&bad1),
            ApplyOutcome::Divergence,
            "the first tampered root must be caught"
        );
        assert_eq!(follower.divergence_count, 1);
        assert_eq!(follower.height(), height_before, "a divergent block must not commit");

        // FAULT #2 — a SECOND, INDEPENDENT tamper, built fresh off a clone of
        // the follower's own still-correct state (not off the producer,
        // which has already diverged ahead by minting `bad1`). This isolates
        // exactly the one-shot-blind-spot question — same divergence path,
        // invoked a second time — from unrelated chain-position bookkeeping.
        let mut honest_view = follower.clone();
        honest_view.enqueue_tx(sign_dummy(sigil_tx::SigilTx::Send {
            from: funded_wallet(3), to: funded_wallet(4), amount: 1, token: sigil_tx::NATIVE, fee: 0,
        }));
        let mut bad2 = honest_view
            .produce_one()
            .expect("cloned honest view can mint the follower's real next block");
        tamper_wallet_root(&mut bad2.header);

        assert_eq!(
            follower.apply_external_block(&bad2),
            ApplyOutcome::Divergence,
            "SECOND, independent divergence must still be caught — a safety net that only \
             fires once is the same class of bug as the heal marker's one-shot limitation"
        );
        assert_eq!(
            follower.divergence_count, 2,
            "both faults must be counted — a stuck-at-1 counter would hide the second event \
             from any operator watching it"
        );
    }
}

// ── THE SCENARIO THIS MODULE WAS WRITTEN TO EVENTUALLY MAKE POSSIBLE ────────────────
//
// Everything below drives the REAL `sigil_block_store::BlockStore` — the exact type
// `sigil-top`'s sync engine uses, not a re-modelled copy. The crate-level tests
// already on `BlockStore` (`put_blocks_bulk_trusted_plants_the_poison_the_checked_
// paths_then_refuse` / `_no_longer_overwrites_an_already_indexed_height`) proved the
// mechanism at ONE hand-picked height. What they cannot answer — because they only
// ever touch one height — is whether the fix's per-height conflict check is
// genuinely LOCAL: does a conflict at one height ever leak into, corrupt, or wedge
// any OTHER height's entry? At chronos scale (thousands of heights, realistic mixed
// honest/conflicting traffic) that question has a real, checkable answer.

/// A minimal, deterministic `SigilBlockHeaderV0` for height `height`, chained to
/// `parent` (the real hash of height-1's HONEST header — `put_block`'s linkage
/// check enforces this, so a fake/zero parent would make every honest insertion
/// past height 0 silently fail as an unlinked header, not a height-index conflict;
/// the linkage-check class of fault already has its own coverage in `sync.rs` and
/// is deliberately kept VALID here so it can never be mistaken for the fault this
/// scenario is actually injecting). `seed` is baked into the VDF input alongside
/// the height, so two headers built with the same `(height, parent)` but different
/// `seed` values hash differently — exactly the shape of a skeleton/live-peer
/// disagreement.
fn mk_wedge_header(height: u64, parent: sigil_header::BlockHash, seed: u64) -> SigilBlockHeaderV0 {
    use sigil_header::*;
    let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
    let mut hh = blake3::Hasher::new();
    hh.update(&parent);
    hh.update(&seed.to_le_bytes());
    hh.update(nonce.as_bytes());
    let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
    let scheme = SigScheme::SqiSign5;
    SigilBlockHeaderV0 {
        version: HEADER_VERSION, network_id: NETWORK_ID, height,
        parent_hash: parent,
        merge_parents: Vec::new(), timestamp_ms: 1_000 + height, nonce_sqisign: nonce,
        vdf_input, vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 }, difficulty: 1,
        wallet_state_root: [0u8; 32], dex_state_root: [0u8; 32], event_log_root: [0u8; 32],
        contract_state_root: [0u8; 32],
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
        txs_merkle_root: [0u8; 32], tx_count: 0,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None,
        },
        sig_scheme: scheme, producer: [0u8; 32],
        producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
        topology_commitment: None,
    }
}

/// Deterministic xorshift64* — no wall-clock RNG in a reproducible harness.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Self(seed | 1) }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Outcome of the scale scenario.
#[derive(Debug, Clone)]
pub struct WedgeScaleResult {
    pub heights_total: u64,
    /// Case B: the honest/checked path landed FIRST, then a conflicting
    /// trusted-path write arrived for the same height. The fix's whole point.
    pub honest_then_trusted_conflict_cases: u64,
    /// Of the case-B heights, how many had their honest entry corrupted by the
    /// later trusted-path conflict. MUST be 0 — this is what the fix guarantees.
    pub honest_entries_corrupted: u64,
    /// Case A: the trusted path landed FIRST (planting a value), then the honest/
    /// checked path tried to write the correct value for the same height. This is
    /// the KNOWN, still-open, order-dependent limitation the module docs above
    /// describe (the fix stops the trusted path from causing NEW poisoning; it
    /// does not retroactively un-poison a height the trusted path got to first).
    pub trusted_then_honest_conflict_cases: u64,
    /// Of the untouched heights — no conflict ever injected there — how many
    /// ended up with the WRONG hash indexed (i.e. contamination that leaked in
    /// from a conflict at some OTHER height). MUST be 0: this is the real question
    /// the single-height unit tests cannot ask.
    pub unrelated_heights_corrupted: u64,
}

/// Drive `total_heights` through a real `BlockStore`, injecting a conflict at every
/// `conflict_every`th height (alternating case A / case B), and confirm: (1) every
/// case-B honest entry survives the later trusted-path conflict untouched, and
/// (2) every height that was NEVER touched by a conflict reads back exactly what an
/// honest-only run would have written — proving the per-height check is genuinely
/// local rather than leaking state across heights (a shared cache, a batch-wide
/// short-circuit, or an off-by-one in the conflict lookup could all fail this even
/// though the single-height unit tests pass).
pub fn wedge_conflicts_stay_local_at_scale(
    total_heights: u64,
    conflict_every: u64,
    seed: u64,
) -> WedgeScaleResult {
    let dir = std::env::temp_dir()
        .join(format!("sigil-chronos-wedge-scale-{}-{}", std::process::id(), seed));
    let _ = std::fs::remove_dir_all(&dir);
    let mut store =
        sigil_block_store::BlockStore::open(&dir.to_string_lossy()).expect("open block store");

    let mut rng = Rng::new(seed);
    let mut honest_then_trusted_conflict_cases = 0u64;
    let mut honest_entries_corrupted = 0u64;
    let mut trusted_then_honest_conflict_cases = 0u64;
    let mut unrelated_heights_corrupted = 0u64;

    // `put_block`'s STRICT downward-linkage check refuses any header whose
    // `parent_hash` doesn't match what's ACTUALLY indexed at height-1 (height 0 is
    // exempt: `height > self.base`). A case-A conflict leaves the POISONED hash
    // indexed at that height, not the honest one — so `parent` for the next height
    // must track whatever is really stored, or every later honest insertion in the
    // whole run would fail the linkage check too, which would test linkage (already
    // covered in `sync.rs`) instead of the height-index-conflict mechanism this
    // scenario exists to isolate.
    let mut parent: sigil_header::BlockHash = [0u8; 32];

    for h in 0..total_heights {
        let honest = mk_wedge_header(h, parent, 0xF00D_0000 + h);
        let honest_hash = honest.hash();
        let is_conflict_height = conflict_every > 0 && h > 0 && h % conflict_every == 0;

        if !is_conflict_height {
            store.put_block(honest.clone()).expect("honest put");
            // Not a conflict site, but may sit downstream of an earlier CASE-A
            // wedge — in which case `parent` is already the poisoned hash and this
            // honest write is EXPECTED to be refused by the (correct) linkage
            // check, not the height-index check this scenario measures. Only
            // count it as unexpected corruption when the chain was clean into
            // this point (parent-matches-what-honest-would-have-produced).
            let stored = store.get_stored_at_height(h).map(|b| b.hash_hex);
            let chain_was_clean = store
                .get_stored_at_height(h.wrapping_sub(1))
                .map(|p| p.hash_hex == hex::encode(parent))
                .unwrap_or(h == 0);
            if chain_was_clean && stored != Some(hex::encode(honest_hash)) {
                unrelated_heights_corrupted += 1;
            }
            parent = stored
                .and_then(|hh| {
                    let mut a = [0u8; 32];
                    hex::decode_to_slice(hh, &mut a).ok().map(|_| a)
                })
                .unwrap_or(honest_hash);
            continue;
        }

        let conflicting = mk_wedge_header(h, parent, 0xBAD0_0000 + rng.next());
        // Alternate ordering so both known cases get real coverage in one run.
        if (h / conflict_every) % 2 == 0 {
            // CASE B — honest first, trusted conflict second. THE fix under test.
            store.put_block(honest.clone()).expect("honest put");
            store.put_blocks_bulk_trusted(&[conflicting]);
            honest_then_trusted_conflict_cases += 1;
            if store.get_stored_at_height(h).map(|b| b.hash_hex) != Some(hex::encode(honest_hash)) {
                honest_entries_corrupted += 1;
            }
            parent = honest_hash; // honest survived untouched — chain continues normally
        } else {
            // CASE A — trusted first (plants), honest second. Known open limitation:
            // the checked path correctly refuses to silently overwrite what's
            // already indexed — see the module docs on why this is a SEPARATE,
            // still-unclosed problem from the one this fix addresses. Recorded as
            // its own count, not conflated with the case-B corruption count above.
            store.put_blocks_bulk_trusted(&[conflicting]);
            let accepted = store.put_block(honest.clone()).expect("honest put attempt");
            if !accepted {
                trusted_then_honest_conflict_cases += 1;
            }
            // Whatever's ACTUALLY indexed now (the poison, if refused as expected)
            // becomes the parent for the next height, so the run keeps exercising
            // fresh heights instead of wedging the whole rest of the scenario —
            // that "the real client stalls forever here" consequence is already
            // established elsewhere and isn't what this loop is measuring.
            parent = store
                .get_stored_at_height(h)
                .and_then(|b| {
                    let mut a = [0u8; 32];
                    hex::decode_to_slice(b.hash_hex, &mut a).ok().map(|_| a)
                })
                .unwrap_or(honest_hash);
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    WedgeScaleResult {
        heights_total: total_heights,
        honest_then_trusted_conflict_cases,
        honest_entries_corrupted,
        trusted_then_honest_conflict_cases,
        unrelated_heights_corrupted,
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    /// THE GATE, at scale. 5,000 heights, a conflict every 7th — hundreds of real
    /// height-index disagreements, not one anecdote. Two things must both hold:
    /// the fix's own guarantee (zero corrupted honest entries) AND the new
    /// question only a scale run can ask (zero contamination of untouched
    /// heights). The known case-A limitation is asserted PRESENT (if it went to
    /// zero, the scenario stopped exercising that path and would be worthless as
    /// a regression guard for it), not absent.
    #[test]
    fn wedge_conflicts_stay_local_at_scale_gate() {
        let r = wedge_conflicts_stay_local_at_scale(5_000, 7, 0xC0FFEE);
        assert!(
            r.honest_then_trusted_conflict_cases > 0,
            "scenario configuration produced zero case-B conflicts — proves nothing"
        );
        assert!(
            r.trusted_then_honest_conflict_cases > 0,
            "scenario configuration produced zero case-A conflicts — the known limitation \
             this scenario also tracks went untested"
        );
        assert_eq!(
            r.honest_entries_corrupted, 0,
            "SECURITY: {} of {} case-B honest entries were corrupted by a later trusted-path \
             conflict — the fix does not hold at scale even though it held at one height",
            r.honest_entries_corrupted, r.honest_then_trusted_conflict_cases
        );
        assert_eq!(
            r.unrelated_heights_corrupted, 0,
            "SECURITY: {} of {} heights that were NEVER touched by an injected conflict came \
             back wrong anyway — the per-height check is leaking state across heights, a class \
             of bug the single-height unit tests structurally cannot see",
            r.unrelated_heights_corrupted, r.heights_total
        );
    }

    /// Sanity floor: with the trusted-path fix reachable via the crate's normal
    /// public API, a run with NO injected conflicts at all must be perfectly clean
    /// — this is what the `conflict_every` heights are being compared against, so
    /// it has to be trustworthy on its own.
    #[test]
    fn no_conflicts_injected_means_zero_corruption() {
        let r = wedge_conflicts_stay_local_at_scale(1_000, 0, 1);
        assert_eq!(r.honest_then_trusted_conflict_cases, 0);
        assert_eq!(r.trusted_then_honest_conflict_cases, 0);
        assert_eq!(r.honest_entries_corrupted, 0);
        assert_eq!(r.unrelated_heights_corrupted, 0);
    }
}
