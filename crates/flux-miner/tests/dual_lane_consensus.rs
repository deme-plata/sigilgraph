//! flux-miner dual-lane PoW consensus properties (Inv: mining fairness).
//!
//! The crate's inline test already covers the happy mine→verify path plus
//! tampered-nonce / tampered-VDF rejection. This file pins the *consensus*
//! properties that keep mining honest and fork-free, none of which the inline
//! test asserts:
//!
//!   - the grind is DETERMINISTIC — same (header, target, vdf_t, group) yields
//!     the identical block, so two honest miners on the same challenge agree;
//!   - the returned nonce is CANONICAL (the smallest satisfying nonce) — the
//!     in-order x8 scan must not skip an earlier valid nonce, which would let a
//!     miner grind for a "nicer" higher nonce;
//!   - difficulty cannot be UNDERSTATED — a block solved for an easy target must
//!     fail verification against any stricter target;
//!   - the GPU/CPU hybrid is CONSISTENT — `block_for_nonce` (CPU rebuild of a
//!     GPU-found nonce) reproduces exactly what `mine_dual` produced, so a
//!     GPU-found share verifies identically.
//!
//! `ModSquaring::bench_2048()` is the reference VDF group; an easy target keeps
//! the grind to a few thousand nonces so the suite stays instant.

use flux_miner::pow::blake4_word_sound;
use flux_miner::{block_for_nonce, mine_dual, verify_dual};
use flux_vdf::ModSquaring;

const HEADER: &[u8] = b"sigil-g0-consensus-test";
const VDF_T: u64 = 1_500;
// Top ~12 bits zero → ~1/4096 hit rate → the canonical nonce is small.
const EASY: u64 = u64::MAX >> 12;

#[test]
fn mine_dual_is_deterministic() {
    let g = ModSquaring::bench_2048();
    let a = mine_dual(HEADER, EASY, VDF_T, &g);
    let b = mine_dual(HEADER, EASY, VDF_T, &g);
    assert_eq!(a.nonce, b.nonce, "nonce must be deterministic");
    assert_eq!(a.blake4_hash, b.blake4_hash, "Lane A word must be deterministic");
    assert_eq!(a.vdf.y, b.vdf.y, "Lane B VDF output must be deterministic");
}

#[test]
fn mined_nonce_is_the_canonical_smallest() {
    let g = ModSquaring::bench_2048();
    let block = mine_dual(HEADER, EASY, VDF_T, &g);
    // No earlier nonce may satisfy the target — the in-order x8 scan must return
    // the FIRST hit. `blake4_word_sound` is the per-lane scalar word the grind
    // uses (byte-identical to the consensus `blake4`).
    for n in 0..block.nonce {
        assert!(
            blake4_word_sound(HEADER, n) > EASY,
            "nonce {n} also satisfies the target but the grind returned {} — non-canonical",
            block.nonce
        );
    }
    assert!(blake4_word_sound(HEADER, block.nonce) <= EASY, "returned nonce must actually solve it");
}

#[test]
fn difficulty_cannot_be_understated() {
    let g = ModSquaring::bench_2048();
    let block = mine_dual(HEADER, EASY, VDF_T, &g);
    assert!(verify_dual(&g, &block, EASY), "must verify at the target it solved");

    // A target stricter than the achieved word must reject the same block —
    // a miner can't claim more difficulty than they actually found.
    if block.blake4_hash > 0 {
        let stricter = block.blake4_hash - 1;
        assert!(
            !verify_dual(&g, &block, stricter),
            "block with word {} wrongly verified against stricter target {stricter}",
            block.blake4_hash
        );
    }
    // And target 0 (only an exact-zero word could pass) must reject too.
    assert!(!verify_dual(&g, &block, 0) || block.blake4_hash == 0);
}

#[test]
fn gpu_cpu_hybrid_is_consistent() {
    let g = ModSquaring::bench_2048();
    let mined = mine_dual(HEADER, EASY, VDF_T, &g);
    // Simulate the GPU half handing the CPU the found nonce: the CPU rebuild
    // must reproduce the identical block (same hash, same VDF) and verify.
    let rebuilt = block_for_nonce(HEADER, mined.nonce, &g, VDF_T);
    assert_eq!(rebuilt.nonce, mined.nonce);
    assert_eq!(rebuilt.blake4_hash, mined.blake4_hash, "CPU rebuild must match the grind's hash");
    assert_eq!(rebuilt.vdf.y, mined.vdf.y, "CPU rebuild must match the grind's VDF");
    assert!(verify_dual(&g, &rebuilt, EASY), "a GPU-found, CPU-rebuilt share must verify");
}

#[test]
fn cross_header_nonce_does_not_verify() {
    // A valid (nonce, VDF) for HEADER must not verify against a DIFFERENT header
    // — the seed binds both lanes to the header, so shares aren't transferable.
    let g = ModSquaring::bench_2048();
    let block = mine_dual(HEADER, EASY, VDF_T, &g);
    let mut wrong = block.clone();
    wrong.header = b"sigil-g0-OTHER-header!!".to_vec();
    assert!(
        !verify_dual(&g, &wrong, EASY),
        "a share must not be replayable under a different header"
    );
}
