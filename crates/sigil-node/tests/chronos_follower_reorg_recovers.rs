//! chronos — a follower orphaned by a producer restart must ROLL BACK to a
//! checkpoint below the fork and rejoin the canonical chain.
//!
//! Reproduces the 2026-09-08 incident against the REAL apply chokepoint: a
//! producer restart rewinds to its snapshot and re-mints DIFFERENT blocks, so a
//! follower that already applied the pre-restart suffix is on an orphaned fork.
//! `ChainTip::apply` is forward-only, so the follower can never extend onto
//! canonical — it wedges, and n=2 finality (which needs both nodes at the tip)
//! freezes. `follower_reorg::ReorgRing` rolls the follower back to the newest
//! checkpoint below the fork; ordinary catch-up then rejoins. If the newest
//! checkpoint is itself on the forked branch, a repeat fork walks strictly older
//! — the rollback self-corrects.
//!
//! Everything here is the real thing: `build_genesis`, `mint_next_block` through
//! the coinbase money chokepoint, and `ChainTip::apply` (header precheck + state
//! commit + root-match). The fork is made with a different coinbase `reward`, so
//! the two branches share an identical prefix and then diverge by state root.
//!
//! Run under `--profile release-fast` (the state-transition STARK wants
//! `debug_assertions` off — same reason the shield prover tests are ignored in
//! debug).

use std::collections::HashMap;

use sigil_node::block::Block;
use sigil_node::chain::ChainTip;
use sigil_node::follower_reorg::ReorgRing;
use sigil_node::mint::mint_next_block;

/// Mint one coinbase-only block on `chain` at its current tip with the given
/// reward, apply it, and return the applied block. `reward` is the fork lever:
/// two branches minted with different rewards share their prefix and then
/// diverge by `wallet_state_root` (hence block hash) at the first differing
/// height.
fn mint_apply(chain: &mut ChainTip, reward: u128) -> Block {
    let (block, _minted) =
        mint_next_block(chain, vec![], &[], Some(reward), None, None, None).expect("mint");
    // Coinbase-only ⇒ no tx rejections, but drain the thread-local queue so
    // nothing leaks between the branches minted in this one test process.
    let _ = sigil_node::coinbase::take_rejections();
    chain.apply(block.clone()).expect("apply own freshly minted block");
    block
}

/// Feed `canon` blocks into `follower` the way the node's backfill drain does:
/// apply the canonical block at the follower's current height; on a
/// parent-hash mismatch, count it and — once it persists — roll back via the
/// ring; repeat. Returns the number of rollbacks performed. Panics if it cannot
/// converge within a generous iteration budget.
fn catch_up_with_reorg(
    follower: &mut ChainTip,
    ring: &mut ReorgRing,
    canon: &HashMap<u64, Block>,
    target_height: u64,
) -> u32 {
    let mut rollbacks = 0u32;
    let mut guard = 0u32;
    let budget = 100_000u32;
    while follower.height() <= target_height {
        guard += 1;
        assert!(guard < budget, "catch-up did not converge (tip H={})", follower.height());
        let want = follower.height();
        let Some(blk) = canon.get(&want).cloned() else {
            panic!("canonical block missing at height {want}");
        };
        match follower.apply(blk) {
            Ok(_) => {
                ring.clear_fork_hits();
                ring.maybe_capture(follower);
            }
            Err(e) => {
                assert!(
                    e.to_string().contains("parent_hash mismatch"),
                    "unexpected apply error (not a fork): {e}"
                );
                if ring.note_fork_hit() {
                    match ring.reorg(follower) {
                        Some(_h) => rollbacks += 1,
                        None => panic!(
                            "ring exhausted at tip H={} — no checkpoint below the fork",
                            follower.height().saturating_sub(1)
                        ),
                    }
                }
            }
        }
    }
    rollbacks
}

#[cfg_attr(debug_assertions, ignore)]
#[test]
fn follower_forked_by_producer_restart_rolls_back_and_rejoins() {
    const R_CANON: u128 = 1_000;
    const R_FORK: u128 = 2_000; // different reward ⇒ divergent state root ⇒ fork
    const PREFIX_F: u64 = 20; // shared common prefix: heights 0..=F
    const FORK_TIP_T: u64 = 35; // follower's orphaned tip
    const CANON_TIP_N: u64 = 50; // canonical chain height we must reach

    // ── shared prefix, minted ONCE so both branches are byte-identical ──────────
    // (Minting it twice would give different wall-clock timestamps → different
    // hashes → no genuine common ancestor. The prefix must be the same object.)
    let mut base = ChainTip::new();
    base.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");

    // The follower owns the ring; capture checkpoints across the prefix so a
    // rollback has a common-chain point to land on.
    let mut ring = ReorgRing::with_params(/*cadence*/ 5, /*cap*/ 20);

    let mut canon: HashMap<u64, Block> = HashMap::new();
    canon.insert(0, base.tip_block().expect("genesis tip").clone());

    for _ in 1..=PREFIX_F {
        let b = mint_apply(&mut base, R_CANON);
        canon.insert(b.header.height, b);
        ring.maybe_capture(&base);
    }

    // ── two branches from the shared tip at height F ────────────────────────────
    let mut canonical = base.clone();
    let mut follower = base; // the follower keeps building the ring

    // Canonical: mint the real suffix F+1..=N (reward R_CANON).
    for _ in (PREFIX_F + 1)..=CANON_TIP_N {
        let b = mint_apply(&mut canonical, R_CANON);
        canon.insert(b.header.height, b);
    }

    // Follower (pre-restart): mint the orphaned suffix F+1..=T (reward R_FORK) —
    // these blocks are valid but DIFFERENT from canonical, capturing as it goes.
    for _ in (PREFIX_F + 1)..=FORK_TIP_T {
        let _ = mint_apply(&mut follower, R_FORK);
        ring.maybe_capture(&follower);
    }

    // Sanity: the two branches really did diverge above the prefix, and agree on it.
    assert_eq!(follower.height(), FORK_TIP_T + 1, "follower is at its forked tip T");
    assert_eq!(canonical.height(), CANON_TIP_N + 1, "canonical is at N");
    assert_ne!(
        follower.tip_block().unwrap().hash(),
        canon[&FORK_TIP_T].hash(),
        "follower tip must differ from canonical at the same height (a real fork)"
    );
    let checkpoints = ring.heights();
    assert!(
        checkpoints.iter().any(|&h| h <= PREFIX_F),
        "must hold at least one checkpoint on the common prefix (≤{PREFIX_F}); have {checkpoints:?}"
    );

    // ── the recovery: feed canonical, expect rollback(s) then convergence ───────
    let rollbacks = catch_up_with_reorg(&mut follower, &mut ring, &canon, CANON_TIP_N);

    assert!(rollbacks >= 1, "a fork must have forced at least one rollback");
    assert_eq!(follower.height(), CANON_TIP_N + 1, "follower reached the canonical tip");
    assert_eq!(
        follower.tip_block().unwrap().hash(),
        canonical.tip_block().unwrap().hash(),
        "follower tip hash == canonical tip hash after recovery"
    );
    // The decisive check: identical STATE, not just identical height/hash.
    assert_eq!(
        follower.roots(),
        canonical.roots(),
        "all four state roots must match canonical after the follower rejoined"
    );
}

/// A follower that is merely BEHIND (no fork — canonical simply extends its tip)
/// must catch up with ZERO rollbacks. Guards against the reorg firing on the
/// normal "we're behind" case, which would be a correctness regression.
#[cfg_attr(debug_assertions, ignore)]
#[test]
fn follower_merely_behind_catches_up_without_any_rollback() {
    const R: u128 = 1_000;
    const N: u64 = 40;

    let mut canonical = ChainTip::new();
    canonical.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("genesis");
    let mut canon: HashMap<u64, Block> = HashMap::new();
    canon.insert(0, canonical.tip_block().unwrap().clone());
    for _ in 1..=N {
        let b = mint_apply(&mut canonical, R);
        canon.insert(b.header.height, b);
    }

    // Follower has only genesis; it is behind, not forked.
    let mut follower = ChainTip::new();
    follower.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("genesis");
    let mut ring = ReorgRing::with_params(5, 20);

    let rollbacks = catch_up_with_reorg(&mut follower, &mut ring, &canon, N);
    assert_eq!(rollbacks, 0, "a behind-but-not-forked follower must never roll back");
    assert_eq!(follower.roots(), canonical.roots(), "state matches after a plain catch-up");
}
