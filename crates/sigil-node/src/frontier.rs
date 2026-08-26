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
///
/// **2026-08-25 — a SECOND, DISTINCT reorg gap found by `sigil-chronos`'s
/// `frontier_memo` harness (real blocks, real `Braid`, engineered ties) and fixed
/// here.** The `applied == 0` signal above only fires when the walk actually finds
/// something to try and fails to chain it on. But the walk starts at `tip` and
/// stops the INSTANT it finds a block shorter than `base` — including `tip`
/// itself. So whenever the braid's real selected tip ends up AT OR BELOW the
/// cached frontier's own height (a same-height, smaller-hash competitor displaced
/// whatever the cache had already committed to, before anything taller was ever
/// minted on top of it), the walk finds NOTHING (`path_len == 0`) and this
/// function returned the stale, wrong cached frontier completely unchanged —
/// silently, with no error, no fallback, nothing. That shape is unreachable from a
/// single always-immediately-minting producer's own local view (mint and cache
/// update are always in lockstep there), which is why the adversarial soak's
/// reorg injections — timed one tick after the contested block, matching that
/// producer's own cadence — never tripped it. It IS reachable for a verifier/
/// follower node that tracks the frontier without minting on every tick, or any
/// gap between "cache the frontier" and "mint on it." Given `path_len == 0` can
/// ONLY happen when `frontier.height() == tip's height + 1` (that is exactly the
/// condition that stops the walk on its first step), the fix is to verify that
/// remaining ambiguity directly: an empty path is trustworthy iff the frontier's
/// own last-applied block IS `tip` itself; if it's not, the tip changed underneath
/// the cache at a height the walk could never see, and this falls back to a full
/// rebuild — the same "never worse than the status quo" guarantee as the
/// `applied == 0` case above.
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
    // An empty path is ONLY the healthy "already caught up" state if the
    // frontier's own tip really IS the braid's current selected tip — see the
    // 2026-08-25 doc note above. `path_len == 0` can only happen when
    // `frontier.height() == <tip's height> + 1` (that's the exact condition that
    // stopped the walk on its first step), so this is the one remaining case the
    // walk itself could never distinguish: same height, different winner.
    if path_len == 0 && frontier.parent_hash() != tip {
        return dag_build_frontier(chain, braid, dag_bodies);
    }
    FrontierBuild { frontier, path_len, applied, fail_reason }
}

// UPDATE 2026-08-25: the fixture this file called for now exists —
// `crates/sigil-chronos/src/frontier_memo.rs` (`sigil-node` is a dev-dependency
// there; see that Cargo.toml entry for why that isn't a cycle). It drives BOTH
// functions above through real blocks via `sigil_node::mint::mint_next_block` +
// real `ChainTip::apply` (real `commit_state_transition`, real four-root checks)
// over a real `Braid` (real `final_depth = 512`), comparing them tick-by-tick
// across a 2,600-tick adversarial soak (129 engineered, verified-winning 2-deep
// reorgs, 103 real finality drains) plus three targeted unit tests. It found one
// real bug in the first version of `dag_build_frontier_memo` — an empty-path case
// the walk could never distinguish from a same-height reorg — which is fixed
// above (see the 2026-08-25 doc note on `dag_build_frontier_memo`) and is now
// covered by a regression test
// (`memo_falls_back_correctly_when_the_spine_reorgs_underneath_an_already_committed_cache`).
// With that fix, all four tests pass, 0 divergences from the baseline across the
// whole soak, and memo's total re-applies were ~63.7k vs baseline's ~1.22M over
// the same run (a real, measured ~19x reduction, not a projection). See that
// file's own module doc for the full mechanism and the soak's exact numbers.
//
// `main.rs` still runs its OWN separate inline copy of the ORIGINAL algorithm,
// completely untouched by any of this — this file remains unreachable from the
// live producer. Chronos-proving the fix is not the same decision as adopting it;
// that adoption call is still open and belongs to a human, given this exact class
// of "looked fine, wasn't" history.
//
// Until then: `main.rs` still runs its OWN inline copy of the original
// algorithm and is untouched. `dag_build_frontier_memo` is not reachable from
// the producer. Nothing here changes live behavior.
