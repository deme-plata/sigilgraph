//! frontier — the producer's speculative frontier build, EXTRACTED so it can
//! be driven by `sigil-chronos` instead of only by the live producer.
//!
//! Why this file exists: on 2026-08-23 a "fix" to this algorithm was written,
//! compiled, and deployed straight to the only live producer. It stopped block
//! production dead and was rolled back. Nothing could have caught it, because
//! the function lived inside `main.rs` — a binary — and no harness can import a
//! binary. Same failure shape as `SyncStore` before it was extracted.
//!
//! Declared `pub mod frontier;` in `lib.rs` AND (later, once a fix is proven)
//! `mod frontier;` in `main.rs` — the two-independent-copies-of-one-source
//! pattern this crate already documents for `coinbase.rs`. `main.rs` keeps its
//! own inline copy until chronos validates a change: prove first, adopt after.
//!
//! THE MEASURED PROBLEM (live, Epsilon + happysrv): `dag_build_frontier`
//! restarts from `chain.clone()` — the SETTLED tip — on every mint tick, and
//! re-applies the entire selected spine from there. The settled tip advances
//! only via the finality drain, which cannot move until the braid's finalized
//! height passes it (`final_depth = 512`). So `base` is pinned for hundreds of
//! blocks and `applied == path_len` climbs in lockstep every tick — logs show
//! 20 -> 35 over 12 minutes, i.e. each tick redoing all prior work plus one.
//! Steady state is ~`final_depth` re-applies PER TICK, forever, not a
//! transient.

use std::collections::HashMap;

use sigil_dagknight::Braid;
use sigil_header::BlockHash;

use crate::block::Block;
use crate::chain::ChainTip;

/// Outcome of one frontier build — what the caller needs plus what a harness
/// needs to measure cost.
#[derive(Clone)]
pub struct FrontierBuild {
    /// The built frontier.
    pub frontier: ChainTip,
    /// Blocks walked from the selected tip down to `base`.
    pub path_len: usize,
    /// Blocks actually re-applied this call — THE cost metric. Under the
    /// current algorithm this equals `path_len` every tick.
    pub applied: usize,
    /// Why the apply loop stopped early, if it did.
    pub fail_reason: &'static str,
}
pub fn dag_build_frontier(
    chain: &ChainTip,
    braid: &Braid,
    dag_bodies: &HashMap<BlockHash, Block>,
) -> FrontierBuild {
    // Verbatim the algorithm running in production (main.rs), with the debug
    // print replaced by returned counters so a harness can measure its cost.
    // Follow the braid's SELECTED SPINE — the parent_hash walk back from
    // selected_tip(). A greedy "first block that fits" walk fails to deepen:
    // height-N siblings are built on DIFFERENT height-(N-1) siblings, so the
    // frontier would cap one above the settled tip and re-mint the same height
    // forever. Walking the ONE selected spine gives a connected parent->child
    // path, so minting extends it by one and finality can advance.
    let mut frontier = chain.clone();
    let Some(tip) = braid.selected_tip() else {
        return FrontierBuild { frontier, path_len: 0, applied: 0, fail_reason: "no-tip" };
    };
    // THE COST: `base` is the settled tip, which only moves when the finality
    // drain lands. Every tick re-walks and re-applies [base, tip] from scratch.
    let base = frontier.height();
    let mut path: Vec<BlockHash> = Vec::new();
    let mut cur = tip;
    loop {
        let Some(b) = dag_bodies.get(&cur) else { break };
        if b.header.height < base { break; }
        path.push(cur);
        cur = b.header.parent_hash;
    }
    let path_len = path.len();
    path.reverse();
    let mut applied = 0usize;
    let mut fail_reason = "";
    for oh in path {
        let Some(b) = dag_bodies.get(&oh) else { fail_reason = "body-missing"; break };
        if b.header.parent_hash == frontier.parent_hash() && b.header.height == frontier.height() {
            if frontier.apply(b.clone()).is_err() {
                fail_reason = "apply-err";
                break;
            }
            applied += 1;
        } else {
            fail_reason = "no-chain";
            break;
        }
    }
    FrontierBuild { frontier, path_len, applied, fail_reason }
}

/// The candidate fix: EXTEND a frontier carried across ticks instead of
/// rebuilding it from the settled tip every time.
///
/// `cached` is the previous tick's frontier. It is only usable when it is at or
/// above the settled tip; otherwise the settled chain has moved past it (a
/// drain landed) and a rebuild is correct.
///
/// SELF-CORRECTING ON REORG: if the cached frontier sits on a spine the braid
/// no longer selects, the walk finds nothing that chains onto it. That is
/// detected here — `applied == 0` with a non-empty path — and the caller gets a
/// full rebuild. A full walk then costs exactly what today's code costs, so a
/// reorg is never worse than the status quo, and the steady path is O(new).
///
/// NOT YET ADOPTED by `main.rs`. A previous attempt at this shape stopped block
/// production live; it is here to be driven by chronos FIRST.
pub fn dag_build_frontier_memo(
    chain: &ChainTip,
    braid: &Braid,
    dag_bodies: &HashMap<BlockHash, Block>,
    cached: Option<&ChainTip>,
) -> FrontierBuild {
    let usable = matches!(cached, Some(c) if c.height() >= chain.height());
    if !usable {
        return dag_build_frontier(chain, braid, dag_bodies);
    }
    let start = cached.expect("usable implies Some");
    let mut frontier = start.clone();
    let Some(tip) = braid.selected_tip() else {
        return FrontierBuild { frontier, path_len: 0, applied: 0, fail_reason: "no-tip" };
    };
    let base = frontier.height();
    let mut path: Vec<BlockHash> = Vec::new();
    let mut cur = tip;
    loop {
        let Some(b) = dag_bodies.get(&cur) else { break };
        if b.header.height < base { break; }
        path.push(cur);
        cur = b.header.parent_hash;
    }
    let path_len = path.len();
    path.reverse();
    let mut applied = 0usize;
    let mut fail_reason = "";
    for oh in path {
        let Some(b) = dag_bodies.get(&oh) else { fail_reason = "body-missing"; break };
        if b.header.parent_hash == frontier.parent_hash() && b.header.height == frontier.height() {
            if frontier.apply(b.clone()).is_err() {
                fail_reason = "apply-err";
                break;
            }
            applied += 1;
        } else {
            fail_reason = "no-chain";
            break;
        }
    }
    // The cached frontier was on a spine the braid abandoned: rebuild.
    // `path_len > 0 && applied == 0` is the precise signal — an EMPTY path
    // means "already caught up", which is the healthy steady state and must
    // NOT trigger a rebuild (mistaking the two is how the live attempt wedged
    // minting: it rebuilt, or failed to, on the wrong condition).
    if path_len > 0 && applied == 0 {
        return dag_build_frontier(chain, braid, dag_bodies);
    }
    FrontierBuild { frontier, path_len, applied, fail_reason }
}

// NO TESTS HERE YET, deliberately.
//
// The cheap thing to write would assert that a walk from tip to base covers
// `tip - base + 1` blocks. That is arithmetic restated, not evidence, and it
// would create a false impression of coverage.
//
// The test that MATTERS is whether `dag_build_frontier_memo` keeps minting
// CORRECT — the previous live attempt at this shape compiled, deployed, and
// stopped block production. Proving that needs real blocks driven through
// `ChainTip::apply`, which validates the four state roots, i.e. the real
// block-building machinery `sigil-chronos`'s `SigilSimNode` already has
// (real apply_tx + commit_state_transition). Wiring that fixture is the next
// step and the gate on adoption.
//
// Until then: `main.rs` still runs its OWN inline copy of the original
// algorithm and is untouched. `dag_build_frontier_memo` is not reachable from
// the producer. Nothing here changes live behavior.
