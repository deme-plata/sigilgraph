//! `frontier_memo` — proves (or disproves) `sigil-node`'s `dag_build_frontier_memo`
//! against the current-production `dag_build_frontier`, through REAL blocks and a
//! REAL `Braid`.
//!
//! ## Why this exists
//!
//! `crates/sigil-node/src/frontier.rs` documents a measured, live production bug:
//! every producer mint tick, `dag_build_frontier` rebuilds the speculative frontier
//! from scratch — walking back to the settled tip (`base`, which only advances
//! every `final_depth` = 512 ticks, when the finality drain lands) and re-applying
//! every block from there. Steady state is ~512 re-applies PER TICK, forever. A
//! candidate fix, `dag_build_frontier_memo`, carries the previous tick's frontier
//! forward and applies only the blocks new since then — O(new) instead of O(window)
//! — with a self-correcting fallback for when the braid's selected spine changes
//! underneath the cached frontier (a reorg): detected as `path_len > 0 && applied
//! == 0`, which triggers a full rebuild (never worse than today's cost).
//!
//! That exact shape was deployed straight to the live producer once before
//! (2026-08-23) and it stopped block production dead — rolled back immediately.
//! `frontier.rs`'s own doc comment names the one thing that must happen before it
//! is ever adopted again: real blocks driven through `ChainTip::apply`, which
//! validates the four state roots — the real block-building machinery, not a toy.
//! This module is that fixture.
//!
//! ## Why sigil-node is a dev-dependency (read before touching this file)
//!
//! `sigil-node` already normal-depends on `sigil-chronos` (its `chronos_sim` /
//! footprint bins). A normal dependency the other way would be a Cargo cycle.
//! `sigil-node` is therefore listed under `[dev-dependencies]` in this crate's
//! Cargo.toml — a pattern Cargo explicitly supports: this test target links the
//! *normal* build of `sigil-node`'s lib, which itself normal-depends on the
//! *normal* build of `sigil-chronos`'s lib. Two distinct, already-deduplicated
//! units; no cycle. Because of this, `lib.rs` declares `#[cfg(test)] mod
//! frontier_memo;` — every item in this file must stay reachable ONLY from a test
//! build. Do not remove that `#[cfg(test)]` gate; doing so turns the dev-dependency
//! trick into a real cycle and the workspace stops compiling.
//!
//! ## What "real" means here
//!
//! Every block in this module is minted via `sigil_node::mint::mint_next_block` —
//! the exact function the live producer calls — which routes its coinbase through
//! `commit_state_transition` (the money chokepoint) and computes the real four
//! state roots. Every application goes through `sigil_node::chain::ChainTip::apply`
//! — the exact function that verifies those roots match what the header claims.
//! The DAG ordering is a real `sigil_dagknight::Braid` (default config, including
//! the real `final_depth = 512`), and reorgs are engineered by inserting genuinely
//! competing sibling blocks — not a mocked braid state.
//!
//! Plain `SigilTx::Send` transactions are deliberately NOT used to vary block
//! content: `sigil_tx::SHIELDED_ONLY_HEIGHT = 0` retires transparent sends from
//! every height this harness could ever reach, so every `apply_tx_at` call would
//! fail closed. Blocks are instead distinguished via `reward_override` (the
//! coinbase amount), which still exercises the exact same `commit_state_
//! transition` chokepoint via the coinbase mutation — the thing this module
//! actually needs to prove correct.
//!
//! ## The one non-obvious thing about the reorg construction (read this before
//! extending the soak below)
//!
//! The memo cache is ALWAYS exactly one slot behind the braid's current selected
//! tip, by construction of the real per-tick pattern: at the top of tick `T`, the
//! cache sits at (height, parent) == (height, parent) of whatever was minted at
//! tick `T-1` — i.e. the cache is exactly AT the fork point of the most recently
//! minted block, not past it. Inserting a single same-height sibling of that most
//! recent block therefore lands in the ORDINARY apply path (parent/height both
//! match the cache directly) — correct, but it never reaches the interesting
//! `path_len > 0 && applied == 0` fallback branch at all.
//!
//! To genuinely exercise the fallback, the competing chain must reach (at least)
//! the SAME height as the current tip while forking ONE GENERATION EARLIER — i.e.
//! a sibling of the tip's PARENT, extended by one more block to tie the tip's own
//! height. That is what [`inject_two_deep_reorg`] builds: `rival_1` (sibling of
//! `lm`'s parent) then `rival_2 = mint(rival_1)` (now level with `lm`), engineered
//! so `rival_2` wins the min-hash tie-break. Only then does the cache's own
//! `parent_hash` point at a block (`lm`'s parent) that the walk from the new
//! selected tip never reaches with a matching link — the real "spine changed
//! underneath an already-committed cache" condition.

use std::collections::HashMap;

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;
use sigil_node::block::Block;
use sigil_node::chain::ChainTip;
use sigil_node::dag::{dag_drain_apply, dag_store_body};
use sigil_node::frontier::{dag_build_frontier, dag_build_frontier_memo, FrontierBuild};
use sigil_node::genesis::build_genesis;
use sigil_node::mint::mint_next_block;
use sigil_state::StateRoots;

/// No eviction pressure in this harness — `dag_drain_apply`'s own `retain()` (below
/// the settled tip) is the only pruning that should matter, matching production.
/// A run of a few thousand ticks plus a few hundred injected siblings is a tiny
/// number of entries either way.
const BODY_CAP: usize = 1_000_000;

/// A cheap, strong fingerprint of a `ChainTip`'s state: height + parent + the four
/// state roots. Two frontiers with an equal fingerprint applied byte-identical
/// history — the roots are BLAKE3/SMT commitments, so this is as strong a
/// correctness signal as comparing the full state, at a fraction of the cost of
/// carrying full clones through thousands of comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    height: u64,
    parent_hash: BlockHash,
    roots: StateRoots,
}

fn fingerprint(c: &ChainTip) -> Fingerprint {
    Fingerprint { height: c.height(), parent_hash: c.parent_hash(), roots: c.roots() }
}

/// Mint a block on top of `frontier` with a distinguishing `reward` — real coinbase,
/// real `commit_state_transition`, real header. No merge parents, no user txs (see
/// the module doc for why txs are deliberately absent).
fn mint(frontier: &ChainTip, reward: u128) -> Block {
    mint_next_block(frontier, vec![], &[], Some(reward), None, None)
        .expect("mint_next_block on a well-formed frontier must succeed")
        .0
}

/// Try up to 4095 distinct reward values, minted on `parent`, until one hashes
/// STRICTLY BELOW `beat`. Returns the winning block. This is how a reorg is made
/// deterministic and adversarial rather than a lucky coin flip: we don't hope a
/// randomly-varied competitor happens to win the braid's min-hash tie-break, we
/// search until we hold one that provably will, then assert the braid agrees.
fn mint_hash_losing_sibling(parent: &ChainTip, beat: BlockHash, reward_base: u128) -> Block {
    for k in 1u128..4096 {
        let candidate = mint(parent, reward_base.wrapping_add(k.wrapping_mul(1_000_003)));
        if candidate.hash() < beat {
            return candidate;
        }
    }
    panic!(
        "could not find a smaller-hash sibling in 4095 tries — either BLAKE3 stopped \
         behaving like a uniform-random oracle, or reward_override stopped changing the \
         header (both would be a much bigger problem than this test)"
    );
}

/// Build and insert a genuine, deterministic, WINNING 2-deep reorg (see the module
/// doc for why it must be 2-deep to actually flip `selected_tip`): `rival_1` is a
/// sibling of `lm_parent_basis`'s own last-applied block (i.e. of `lm`'s PARENT,
/// not of `lm` itself), and `rival_2 = mint(rival_1)` ties `lm`'s height. Engineered
/// so `rival_2.hash() < lm.hash()`, then confirms the braid actually agrees.
/// Returns `rival_2` (the new — verified — selected tip).
fn inject_two_deep_reorg(
    braid: &mut Braid,
    dag_bodies: &mut HashMap<BlockHash, Block>,
    grandparent_basis: &ChainTip,
    lm: &Block,
    reward_base: u128,
) -> Block {
    let rival_1 = mint(grandparent_basis, reward_base.wrapping_add(11));
    assert!(
        !matches!(braid.insert(BlockView::from(&rival_1.header)), InsertOutcome::Rejected(_)),
        "an honestly-constructed sibling-of-the-parent must be accepted by the braid"
    );
    dag_store_body(dag_bodies, BODY_CAP, rival_1.hash(), rival_1.clone());

    let mut rival_1_frontier = grandparent_basis.clone();
    rival_1_frontier.apply(rival_1.clone()).expect("applying rival_1 onto its own parent basis must succeed");
    let rival_2 = mint_hash_losing_sibling(&rival_1_frontier, lm.hash(), reward_base.wrapping_add(13));
    assert!(
        !matches!(braid.insert(BlockView::from(&rival_2.header)), InsertOutcome::Rejected(_)),
        "an honestly-constructed 2-deep rival must be accepted by the braid"
    );
    dag_store_body(dag_bodies, BODY_CAP, rival_2.hash(), rival_2.clone());

    assert_eq!(
        braid.selected_tip(),
        Some(rival_2.hash()),
        "engineered 2-deep rival did not win the min-hash tie-break at the tied height — the \
         injection is not testing what it claims to"
    );
    rival_2
}

/// Fresh, genesis-only `(ChainTip, Braid, dag_bodies)` triple — the shared ground
/// truth every scenario in this module starts from. `Braid::default()` keeps the
/// real `final_depth = 512`, not a shrunk test value, so the soak's cost profile is
/// the real one.
fn fresh_world() -> (ChainTip, Braid, HashMap<BlockHash, Block>) {
    let genesis = build_genesis().expect("genesis");
    let mut chain = ChainTip::new();
    chain.apply(genesis.clone()).expect("apply genesis");
    let mut braid = Braid::new(BraidConfig::default());
    let mut dag_bodies = HashMap::new();
    assert!(
        !matches!(braid.insert(BlockView::from(&genesis.header)), InsertOutcome::Rejected(_)),
        "genesis must be accepted into a fresh braid"
    );
    dag_store_body(&mut dag_bodies, BODY_CAP, genesis.hash(), genesis);
    (chain, braid, dag_bodies)
}

/// Everything the soak measured, for the caller to print/assert on.
#[derive(Debug, Default)]
pub struct SoakResult {
    pub ticks: u64,
    pub reorgs_injected: u64,
    pub reorgs_confirmed_won_tiebreak: u64,
    pub drains_run: u64,
    pub baseline_applied_total: u64,
    pub memo_applied_total: u64,
    /// Ticks where memo's frontier disagreed with baseline's. MUST be empty.
    pub divergent_ticks: Vec<u64>,
    pub final_settled_height: u64,
    pub final_frontier_height: u64,
}

/// Drive `ticks` mint-ticks of a real producer loop over a real `Braid`, comparing
/// `dag_build_frontier` (baseline, full-rebuild-every-tick, the current-production
/// algorithm) against `dag_build_frontier_memo` (candidate, carries the previous
/// tick's frontier forward) at EVERY SINGLE TICK — not just at the end.
///
/// Every `reorg_every` ticks (when `reorg_every > 0` and enough history exists), a
/// genuine 2-deep competing chain (see [`inject_two_deep_reorg`] / the module doc)
/// is inserted into the SAME braid, engineered to win the min-hash tie-break at the
/// height it ties. This is the exact "spine changed underneath an already-
/// committed cache" condition the module doc says the live 2026-08-23 attempt got
/// backwards.
///
/// Every `drain_every` ticks (when `drain_every > 0`), the real finality drain
/// (`dag_drain_apply`) runs, advancing the settled `chain` along whatever the
/// braid has actually finalized — the real trigger for `base` (and therefore
/// baseline's cost) to jump forward, and the real test of memo's `cached.height()
/// >= chain.height()` usability guard staying true across a live settle.
pub fn run_soak(ticks: u64, reorg_every: u64, drain_every: u64, seed: u128) -> SoakResult {
    let (mut chain, mut braid, mut dag_bodies) = fresh_world();

    let mut memo_cached: Option<ChainTip> = None;
    // `prev1` = (the block minted+inserted at the end of the previous tick, the
    // frontier it was minted FROM). `prev2_parent` = the frontier the block minted
    // TWO ticks ago was minted from — i.e. one generation earlier than `prev1`'s
    // own parent-basis. Needed to fork a sibling of `prev1`'s PARENT (see the
    // module doc for why this, and not a sibling of `prev1` itself, is what
    // actually exercises the fallback).
    let mut prev1: Option<(Block, ChainTip)> = None;
    let mut prev2_parent: Option<ChainTip> = None;

    let mut mint_hash_to_tx_hashes: HashMap<BlockHash, Vec<[u8; 32]>> = HashMap::new();
    let send_bridge = sigil_api::send::SendBridge::new();
    let bridge_bridge = sigil_api::bridge::BridgeBridge::new(None, None);
    let dex_bridge = sigil_api::dex::DexBridge::new();
    let usds_bridge = sigil_api::usds::UsdsBridge::new();
    let usds_polygon_bridge = sigil_api::usds_bridge::UsdsBridgeBridge::new(None, None);
    let shielded_bridge = sigil_api::shielded::ShieldedBridge::new();

    let mut r = SoakResult { ticks, ..Default::default() };

    for tick in 0..ticks {
        // (a) Inject a reorg BEFORE this tick's frontier computation — the moment a
        // real, already-2-deep competing branch (e.g. from a concurrent producer)
        // would have landed via gossip. Requires two full ticks of history.
        if reorg_every > 0 && tick > 0 && tick % reorg_every == 0 {
            if let (Some((lm, _lm_parent)), Some(gp)) = (&prev1, &prev2_parent) {
                let _rival_2 = inject_two_deep_reorg(
                    &mut braid,
                    &mut dag_bodies,
                    gp,
                    lm,
                    seed.wrapping_add(tick as u128).wrapping_mul(97),
                );
                r.reorgs_injected += 1;
                r.reorgs_confirmed_won_tiebreak += 1; // inject_two_deep_reorg asserts this internally
            }
        }

        // (b) THE GATE. Baseline is the ground truth (full rebuild, unconditionally
        // correct by construction). Memo carries `memo_cached` forward. Both read
        // the identical, just-possibly-reorged shared state.
        let baseline: FrontierBuild = dag_build_frontier(&chain, &braid, &dag_bodies);
        r.baseline_applied_total += baseline.applied as u64;
        let memo: FrontierBuild = dag_build_frontier_memo(&chain, &braid, &dag_bodies, memo_cached.as_ref());
        r.memo_applied_total += memo.applied as u64;

        let bfp = fingerprint(&baseline.frontier);
        let mfp = fingerprint(&memo.frontier);
        if bfp != mfp {
            r.divergent_ticks.push(tick);
        }

        // (c) Mint this tick's own block on the CORRECTED, canonical frontier —
        // guarantees the chain keeps progressing correctly regardless of what just
        // happened to memo, so a real divergence (if found) is isolated to the
        // report rather than cascading the rest of the run into noise.
        let primary = mint(&baseline.frontier, seed.wrapping_add(tick as u128).wrapping_add(7));
        assert!(
            !matches!(braid.insert(BlockView::from(&primary.header)), InsertOutcome::Rejected(_)),
            "tick {tick}: own well-formed candidate must be accepted by the braid"
        );
        dag_store_body(&mut dag_bodies, BODY_CAP, primary.hash(), primary.clone());

        prev2_parent = prev1.as_ref().map(|(_, pf)| pf.clone());
        prev1 = Some((primary, baseline.frontier.clone()));
        memo_cached = Some(memo.frontier.clone());

        // (d) Periodic real finality drain — advances `chain` along whatever the
        // braid has actually finalized (a no-op until the spine clears
        // `final_depth` = 512 above the settled tip; the run must span a good
        // multiple of that for this to fire more than a handful of times).
        if drain_every > 0 && tick > 0 && tick % drain_every == 0 {
            let (_applied, _skipped, failed) = dag_drain_apply(
                &mut braid,
                &mut dag_bodies,
                &mut chain,
                &mut |_raw: &[u8]| {},
                &send_bridge,
                &bridge_bridge,
                &dex_bridge,
                &usds_bridge,
                &usds_polygon_bridge,
                &shielded_bridge,
                &mut mint_hash_to_tx_hashes,
            );
            assert_eq!(failed, 0, "tick {tick}: a finality drain must never fail through the real chokepoint");
            r.drains_run += 1;
        }
    }

    r.final_settled_height = chain.height();
    r.final_frontier_height = memo_cached.map(|c| c.height()).unwrap_or(chain.height());
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE MAIN GATE. A real, adversarial soak: thousands of mint ticks, dozens of
    /// engineered 2-deep reorgs, periodic real finality drains, spanning several
    /// multiples of the real `final_depth = 512` window. Prints the real measured
    /// cost so the numbers land in the test log (`--nocapture`) rather than only
    /// in an assert message.
    #[test]
    fn memo_matches_baseline_across_a_long_adversarial_soak() {
        // ~5x final_depth (512), dozens of reorgs, frequent real drains. reorg_every
        // and drain_every are deliberately NOT the same cadence (20 vs 25) so a
        // reorg tick and a drain tick don't always coincide.
        let r = run_soak(2_600, 20, 25, 0xF00D_5161_0000_0001);

        eprintln!(
            "frontier_memo soak: ticks={} reorgs_injected={} reorgs_confirmed_won_tiebreak={} \
             drains_run={} baseline_applied_total={} memo_applied_total={} \
             final_settled_height={} final_frontier_height={} divergent_ticks={}",
            r.ticks,
            r.reorgs_injected,
            r.reorgs_confirmed_won_tiebreak,
            r.drains_run,
            r.baseline_applied_total,
            r.memo_applied_total,
            r.final_settled_height,
            r.final_frontier_height,
            r.divergent_ticks.len(),
        );

        assert!(r.reorgs_injected >= 100, "scenario configuration produced too few reorgs to be adversarial");
        assert_eq!(
            r.reorgs_injected, r.reorgs_confirmed_won_tiebreak,
            "every injected rival must have actually won the tie-break — otherwise some \
             \"reorgs\" tested nothing"
        );
        assert!(r.drains_run > 0, "scenario configuration never exercised a real finality drain");
        assert!(
            r.divergent_ticks.is_empty(),
            "SECURITY: dag_build_frontier_memo disagreed with dag_build_frontier at {} of {} \
             ticks (first few: {:?}) — the candidate fix is NOT safe to adopt as-is",
            r.divergent_ticks.len(),
            r.ticks,
            &r.divergent_ticks[..r.divergent_ticks.len().min(10)],
        );
        // The whole point: memo's total cost must be close to the number of ticks
        // (~O(1)/tick — one new block per tick, plus the handful of full rebuilds
        // reorgs force), while baseline's must be far larger (it re-applies the
        // whole pending window on every single tick). If memo's total ever creeps
        // up near baseline's, the "fix" isn't actually fixing the cost shape.
        assert!(
            r.memo_applied_total < r.baseline_applied_total / 4,
            "memo's total re-applies ({}) is not dramatically smaller than baseline's ({}) — \
             the candidate is not delivering the O(new) cost shape it claims",
            r.memo_applied_total,
            r.baseline_applied_total,
        );
    }

    /// THE STEADY-STATE GATE. The module doc for `dag_build_frontier_memo` names
    /// the precise failure class the live 2026-08-23 attempt got backwards:
    /// mistaking an EMPTY path (already caught up — the healthy, common case) for
    /// a reorg. This isolates exactly that: catch a cache up to the real tip, call
    /// again with NOTHING new having arrived, and require a true no-op — zero
    /// applied, `fail_reason` stays empty (not `"no-chain"`), and the returned
    /// frontier is the cached one unchanged, not a rebuild that merely happens to
    /// agree.
    #[test]
    fn memo_does_not_mistake_an_empty_path_for_a_reorg() {
        let (chain, mut braid, mut dag_bodies) = fresh_world();

        let base = dag_build_frontier(&chain, &braid, &dag_bodies);
        assert_eq!(base.path_len, 0, "nothing beyond genesis exists yet");

        let b1 = mint(&base.frontier, 111);
        assert!(!matches!(braid.insert(BlockView::from(&b1.header)), InsertOutcome::Rejected(_)));
        dag_store_body(&mut dag_bodies, BODY_CAP, b1.hash(), b1.clone());

        let caught_up = dag_build_frontier_memo(&chain, &braid, &dag_bodies, Some(&base.frontier));
        assert_eq!(caught_up.applied, 1, "must have picked up b1");
        assert_eq!(caught_up.fail_reason, "");
        assert_eq!(fingerprint(&caught_up.frontier).height, b1.header.height + 1);

        // Nothing new has arrived. This is the healthy, overwhelmingly common
        // per-tick case (a producer that, for whatever reason, calls the frontier
        // builder without having minted since the last call).
        let steady = dag_build_frontier_memo(&chain, &braid, &dag_bodies, Some(&caught_up.frontier));
        assert_eq!(steady.path_len, 0, "no new block exists past the cache — path must be empty");
        assert_eq!(steady.applied, 0);
        assert_eq!(
            steady.fail_reason, "",
            "an empty path must never be reported as a failed/no-chain walk — that is exactly \
             the confusion the module doc says wedged production live"
        );
        assert_eq!(
            fingerprint(&steady.frontier),
            fingerprint(&caught_up.frontier),
            "a healthy no-op tick must return the cached frontier unchanged, not trigger a rebuild"
        );
    }

    /// A same-height sibling of the MOST RECENTLY minted block lands in the
    /// ORDINARY apply path, not the fallback — because the cache is always exactly
    /// AT that fork point (see the module doc). Worth pinning explicitly: it shows
    /// the memo function correctly resolves "two options at a fork it already
    /// knows about" through the plain path, distinguishing that from the deeper
    /// case in the next test.
    #[test]
    fn memo_resolves_a_same_tick_sibling_through_the_ordinary_path() {
        let (chain, mut braid, mut dag_bodies) = fresh_world();

        let base = dag_build_frontier(&chain, &braid, &dag_bodies);
        let cached_at_fork = base.frontier.clone();

        let mine = mint(&base.frontier, 222);
        assert!(!matches!(braid.insert(BlockView::from(&mine.header)), InsertOutcome::Rejected(_)));
        dag_store_body(&mut dag_bodies, BODY_CAP, mine.hash(), mine.clone());

        let rival = mint_hash_losing_sibling(&base.frontier, mine.hash(), 9_000);
        assert!(!matches!(braid.insert(BlockView::from(&rival.header)), InsertOutcome::Rejected(_)));
        dag_store_body(&mut dag_bodies, BODY_CAP, rival.hash(), rival.clone());
        assert_eq!(braid.selected_tip(), Some(rival.hash()), "rival must win the tie-break");

        let result = dag_build_frontier_memo(&chain, &braid, &dag_bodies, Some(&cached_at_fork));
        assert_eq!(result.applied, 1, "resolved through the ordinary single-apply path, not a rebuild");
        assert_eq!(result.fail_reason, "");
        assert_eq!(fingerprint(&result.frontier).parent_hash, rival.hash());

        let expected = dag_build_frontier(&chain, &braid, &dag_bodies);
        assert_eq!(fingerprint(&result.frontier), fingerprint(&expected.frontier));
    }

    /// THE REORG GATE. Builds the real "cache has already committed past a block
    /// that then gets orphaned" condition (see the module doc for the full
    /// derivation of why this needs a 2-deep competing chain, not a single
    /// sibling): `lm` already has the cache built on top of it (`cache_on_lm`,
    /// parent == `lm.hash()`); a rival to `lm`'s OWN parent then catches up to
    /// `lm`'s height and wins. The cache's `parent_hash` now points at nothing on
    /// the winning spine's reachable path — the exact `path_len > 0 && applied ==
    /// 0` condition — and the memo call must land on EXACTLY what an independent
    /// full rebuild computes, built on the WINNING chain, not the orphaned one.
    #[test]
    fn memo_falls_back_correctly_when_the_spine_reorgs_underneath_an_already_committed_cache() {
        let (chain, mut braid, mut dag_bodies) = fresh_world();

        // Two ticks of honest history: `parent_of_p` -> p -> lm.
        let base = dag_build_frontier(&chain, &braid, &dag_bodies);
        let parent_of_p = base.frontier.clone();
        let p = mint(&parent_of_p, 10);
        assert!(!matches!(braid.insert(BlockView::from(&p.header)), InsertOutcome::Rejected(_)));
        dag_store_body(&mut dag_bodies, BODY_CAP, p.hash(), p.clone());

        let after_p = dag_build_frontier(&chain, &braid, &dag_bodies);
        let lm = mint(&after_p.frontier, 20);
        assert!(!matches!(braid.insert(BlockView::from(&lm.header)), InsertOutcome::Rejected(_)));
        dag_store_body(&mut dag_bodies, BODY_CAP, lm.hash(), lm.clone());

        // The cache, per the real per-tick pattern, is now built exactly on `lm`
        // (one tick after `lm` was minted, it was absorbed on the following
        // memo call — reproduced directly here rather than via the full loop).
        let cache_on_lm = dag_build_frontier_memo(&chain, &braid, &dag_bodies, Some(&after_p.frontier));
        assert_eq!(cache_on_lm.applied, 1);
        assert_eq!(fingerprint(&cache_on_lm.frontier).parent_hash, lm.hash());

        // A 2-deep rival, forking at `p` (NOT at `lm`), catches up to `lm`'s
        // height and is engineered to win.
        let rival_2 = inject_two_deep_reorg(&mut braid, &mut dag_bodies, &parent_of_p, &lm, 5_000);

        // The memo call is handed the cache that already committed to `lm`. The
        // spine has genuinely changed underneath it — `lm` is no longer reachable
        // from the new selected tip via a matching parent link.
        let result = dag_build_frontier_memo(&chain, &braid, &dag_bodies, Some(&cache_on_lm.frontier));
        let expected = dag_build_frontier(&chain, &braid, &dag_bodies);
        assert_eq!(
            fingerprint(&result.frontier),
            fingerprint(&expected.frontier),
            "memo must land on exactly what a full rebuild computes once the spine reorgs \
             underneath an already-committed cache"
        );
        assert_eq!(
            fingerprint(&result.frontier).parent_hash,
            rival_2.hash(),
            "the corrected frontier must be built on the WINNING chain, not the orphaned one"
        );
        assert_ne!(
            fingerprint(&result.frontier).parent_hash,
            lm.hash(),
            "sanity: the orphaned block must not still be what the frontier is built on"
        );
    }
}
