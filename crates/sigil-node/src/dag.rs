//! dag.rs — DagKnight/GHOSTDAG braid wiring: seed, frontier, drain/settle, and the
//! QTFT topology commitment. Shared by the `sigil-node` binary and any external
//! crate that needs to run the SAME braid logic (e.g. `sigil-top`'s `producer`
//! feature — see `producer/dag.rs` there).
//!
//! 2026-08-23 (grogu-producer-unification Phase 2): moved out of `main.rs` so
//! sigil-top can call the REAL braid functions instead of maintaining a
//! hand-ported duplicate that could silently drift from what the live producer
//! actually runs. Dual-declared in both `main.rs` (`mod dag;`) and `lib.rs`
//! (`pub mod dag;`) — same pattern as `genesis`/`coinbase`/`producer_signing`.
//! Every function here takes its state as explicit parameters (chain, braid,
//! dag_bodies, the bridges) rather than closing over anything main.rs-local, so
//! the move is a pure relocation — zero behavior change, verified by running
//! sigil-node's existing DAG test group after the move.

use std::collections::{BTreeMap, HashMap};

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;

use crate::block::Block;
use crate::chain::ChainTip;

/// Seed a fresh `Braid` from the local chain's in-RAM window. A pruned window
/// (`chain.window_base() > 0`) anchors the braid at the oldest in-RAM block via
/// `Braid::new_with_base` — trusted because this node applied it through the
/// state chokepoint — so it chains cleanly instead of parking against unknown
/// pre-window ancestry.
pub fn dag_seed_braid(chain: &ChainTip) -> Braid {
    let cfg = BraidConfig::from_env();
    eprintln!("🕸 braid config: final_depth={} max_window={} max_pending={} max_merge_parents={} ghostdag_k={}",
        cfg.final_depth, cfg.max_window, cfg.max_pending, cfg.max_merge_parents,
        cfg.ghostdag_k.map(|k| k.to_string()).unwrap_or_else(|| "off (v1)".to_string()));
    let base_h = chain.window_base();
    let (mut b, seed_from) = if base_h > 0 {
        match chain.get(base_h) {
            Some(base_blk) => {
                let bv = BlockView::from(&base_blk.header);
                eprintln!("🕸 braid base-anchored at H={} (pruned window; pre-window ancestry trusted-local)", base_h);
                (Braid::new_with_base(cfg, bv.hash, bv.height), base_h + 1)
            }
            None => (Braid::new(cfg), base_h),
        }
    } else {
        (Braid::new(cfg), 0)
    };
    let mut seeded = 0usize;
    for hh in seed_from..chain.height() {
        if let Some(blk) = chain.get(hh) {
            let view = BlockView::from(&blk.header);
            // Window blocks may carry merge edges to bodies this node never
            // stored (the other strand, pre-catch-up). Those references are
            // committed inside headers we applied through the chokepoint —
            // anchor them as trusted history so the seed chains instead of
            // cascading into the parked set.
            for mp in &view.merge_parents {
                b.anchor_trusted(*mp, view.height.saturating_sub(1));
            }
            if matches!(b.insert(view), InsertOutcome::Inserted { .. }) {
                seeded += 1;
            }
        }
    }
    eprintln!("🕸 braid seeded with {} local window blocks", seeded);
    b
}

/// DAGKnight: build the speculative *frontier* the producer mints on — a clone of
/// the settled chain with the braid's pending selected-spine suffix applied on top.
/// The settled chain advances ONLY via the finalized `drain_ordered()` order (so every
/// node converges); but a producer must build on a tip *ahead* of finality, and its
/// coinbase roots must match what the drain will recompute — which they do, because the
/// frontier applies the same selected-spine blocks the drain will. Rebuilt each tick →
/// reorg-safe. Blocks below the settled tip won't extend the frontier and are skipped.
pub fn dag_build_frontier(
    chain: &ChainTip,
    braid: &Braid,
    dag_bodies: &HashMap<BlockHash, Block>,
) -> ChainTip {
    let mut frontier = chain.clone();
    // Follow the braid's SELECTED SPINE — the `parent_hash` walk back from
    // `selected_tip()` (max-height, min-hash). A greedy "first block that fits"
    // walk fails to deepen: with two producers, height-N siblings are built on
    // DIFFERENT height-(N-1) siblings, so a random height-1 pick has no matching
    // height-2 child → the frontier caps one above the settled tip and every tick
    // re-mints the same height forever (the DAG stays a flat bush, never clears
    // final_depth, nothing finalizes). Walking the ONE selected spine gives a
    // connected parent→child path, so the frontier reaches the selected tip and
    // minting extends it by one → the spine deepens → finality advances.
    let dbg = std::env::var("SIGIL_FRONTIER_DEBUG").ok().as_deref() == Some("1");
    let Some(tip) = braid.selected_tip() else {
        if dbg { let s = braid.stats(); eprintln!("🔬 frontier: selected_tip=None base={} window={} pending={} tips={} rejected={} dropped={}", frontier.height(), s.window, s.pending, s.tips, s.rejected, s.dropped); }
        return frontier;
    };
    let tip_h = dag_bodies.get(&tip).map(|b| b.header.height as i64).unwrap_or(-1);
    // Collect the spine bodies from the selected tip back down to (not including)
    // the settled tip's height, then apply in ascending height order.
    let base = frontier.height(); // first height the frontier still needs
    let mut path: Vec<BlockHash> = Vec::new();
    let mut cur = tip;
    loop {
        let Some(b) = dag_bodies.get(&cur) else { break };
        if b.header.height < base { break; } // reached the settled region
        path.push(cur);
        cur = b.header.parent_hash;
    }
    let path_len = path.len();
    path.reverse(); // ascending height, spine-connected
    let mut applied_n = 0usize;
    let mut fail_reason = "";
    for oh in path {
        let Some(b) = dag_bodies.get(&oh) else { fail_reason = "body-missing"; break };
        if b.header.parent_hash == frontier.parent_hash() && b.header.height == frontier.height() {
            if let Err(e) = frontier.apply(b.clone()) {
                fail_reason = "apply-err";
                if dbg { eprintln!("🔬 frontier apply-err at h={}: {}", b.header.height, e); }
                break;
            }
            applied_n += 1;
        } else {
            fail_reason = "no-chain";
            break; // spine block doesn't chain onto the frontier — stop cleanly
        }
    }
    if dbg {
        let s = braid.stats();
        eprintln!("🔬 frontier: tip_h={} base={} path_len={} applied={} fail={} → frontier_h={} | window={} pending={} tips={} emitted={} fin_h={} rej={} drop={}",
            tip_h, base, path_len, applied_n, fail_reason, frontier.height(),
            s.window, s.pending, s.tips, s.emitted_total, s.finalized_height, s.rejected, s.dropped);
    }
    frontier
}

/// SIGIL_DAG=1 drain step (design §3.2 step 4): pull the braid's newly
/// finalized order and state-apply EXACTLY the blocks that extend the local
/// tip (`parent_hash == tip && height == next`) through the full, UNMODIFIED
/// `ChainTip::apply` chokepoint (precheck → commit_state_transition →
/// check_roots_match). Ordered blocks that are off-spine, non-extending, or
/// already applied (the producer self-applies its own) are counted in
/// `skipped` and NOT state-applied — v0 semantics per
/// `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §3.4. Applied blocks are handed to
/// `persist` (the chain_log append path) exactly as the linear path does.
/// Bodies below the braid's finalized height are evicted after the drain.
/// Returns `(applied, skipped, failed)`.
#[allow(clippy::too_many_arguments)]
pub fn dag_drain_apply(
    braid: &mut Braid,
    dag_bodies: &mut HashMap<BlockHash, Block>,
    chain: &mut ChainTip,
    persist: &mut dyn FnMut(&[u8]),
    send_bridge: &sigil_api::send::SendBridge,
    bridge_bridge: &sigil_api::bridge::BridgeBridge,
    dex_bridge: &sigil_api::dex::DexBridge,
    usds_bridge: &sigil_api::usds::UsdsBridge,
    usds_polygon_bridge: &sigil_api::usds_bridge::UsdsBridgeBridge,
    // 2026-08-24: was missing entirely — every OTHER bridge gets `confirm_applied`
    // here, but shielded's never did, so a landed RegisterShieldedAddress/
    // ShieldedSend/Shield/Unshield tx never retired from `ShieldedBridge.pending`
    // and kept riding along on every future candidate (harmless for the idempotent
    // register case, but pure waste, and actively wrong for a landed ShieldedSend —
    // its nullifier is now spent, so every resubmission attempt would fail proof
    // re-verification until it separately timed out). Found alongside the MAX_AGE
    // fix (see `shielded.rs`'s constant) while root-causing why registration never
    // took effect — this alone would not have fixed that (the tx was expiring
    // client-side long before finality, never reaching this function's `Ok` arm at
    // all), but it's required for a tx that DOES land to behave correctly once the
    // MAX_AGE fix lets it actually reach finalization.
    shielded_bridge: &sigil_api::shielded::ShieldedBridge,
    mint_hash_to_tx_hashes: &mut HashMap<BlockHash, Vec<[u8; 32]>>,
) -> (u64, u64, u64) {
    // Cross-node divergence detector (opt-in, test meshes): log every
    // finalized emission with its sequence index. Two nodes' ⛓ streams must
    // be prefix-identical — the wire analogue of sim gate S1.
    let order_log = std::env::var("SIGIL_DAG_ORDER_LOG").ok().as_deref() == Some("1");
    let (mut applied, mut skipped, mut failed) = (0u64, 0u64, 0u64);
    // Advance the braid's frozen order + slide its retention window (side
    // effects: emit/cleanup). The returned Kahn batch is NOT what we settle on
    // — it interleaves sibling forks, and picking "first block that extends"
    // out of it follows a DIFFERENT fork than `dag_build_frontier` (which walks
    // `selected_tip`). When those two disagree at a fork the settled chain and
    // the frontier stall on opposite branches. So settlement follows the SAME
    // canonical spine as the frontier: the `selected_tip` parent-walk, applied
    // only up to `finalized_height`. Deterministic per DAG ⇒ every node lands
    // the identical settled chain.
    let _ = braid.drain_ordered();
    let fin = braid.finalized_height();
    let base = chain.height();
    // Walk the selected spine from the tip down, keeping the finalized band
    // [base, fin]; then apply ascending so each block extends the settled tip.
    let mut path: Vec<BlockHash> = Vec::new();
    if let Some(tip) = braid.selected_tip() {
        let mut cur = tip;
        loop {
            let Some(b) = dag_bodies.get(&cur) else { break };
            let h = b.header.height;
            if h < base { break; } // reached the already-settled region
            if h <= fin { path.push(cur); } // only the finalized prefix settles
            cur = b.header.parent_hash;
        }
    }
    path.reverse();
    let mut ord_idx = chain.height();
    for oh in path {
        let Some(body) = dag_bodies.get(&oh) else { skipped += 1; continue };
        if body.header.parent_hash != chain.parent_hash() || body.header.height != chain.height() {
            skipped += 1; // not spine-contiguous with the settled tip (transient)
            continue;
        }
        if order_log {
            eprintln!("⛓ ord #{} {}", ord_idx, hex::encode(&oh[..12]));
        }
        ord_idx += 1;
        // Persisted form goes through the ONE chain-log encoder (msgpack+zstd, 4.4x
        // smaller than the JSON this used to write). The P2P wire is unaffected — that
        // still speaks JSON, and is encoded separately at its own call sites.
        let braw = crate::chain_log::encode_record(body).unwrap_or_default();
        match chain.apply(body.clone()) {
            Ok(()) => {
                persist(&braw);
                applied += 1;
                // THIS candidate — and no other same-height sibling — just landed on
                // the settled spine. Retire exactly the sends it carried; anything
                // still in SendBridge's pending map (including sends that rode along
                // on an orphaned sibling of this same height) stays pending and rides
                // the next mint attempt.
                if let Some(hashes) = mint_hash_to_tx_hashes.remove(&oh) {
                    send_bridge.confirm_applied(&hashes);
                    bridge_bridge.confirm_applied(&hashes);
                    dex_bridge.confirm_applied(&hashes);
                    usds_bridge.confirm_applied(&hashes);
                    usds_polygon_bridge.confirm_applied(&hashes);
                    shielded_bridge.confirm_applied(&hashes);
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("🔴 braid spine apply FAILED at H={} — {}", body.header.height, e);
            }
        }
    }
    // Retain bodies from the settled tip upward (the frontier needs the pending
    // spine suffix); anything below the new settled height is spent.
    let keep = chain.height().saturating_sub(1);
    dag_bodies.retain(|_, b| b.header.height >= keep);
    (applied, skipped, failed)
}

/// Hard cap on the height-keyed `pending` block buffer, and how far ahead of the
/// local tip a gossiped block may be buffered at all.
///
/// **Why this exists (measured, 2026-08-01).** `pending` is a
/// `BTreeMap<u64, Block>` fed from FOUR sites. Only the legacy linear path was
/// capped (`if pending.len() < 200_000`); the **braid gossip path** and both
/// backfill-response paths inserted with no bound beyond `h >= chain.height()`.
/// The live node runs `SIGIL_DAG=1`, i.e. the uncapped path.
///
/// A `sigil-memwedge` chronos run measured the true cost at **2,803 bytes per
/// buffered height** (header only — a full Block is larger). So a node that
/// falls behind buffers the whole gap: 1M heights ≈ 2.6 GiB, 10M ≈ 26 GiB,
/// against a `MemoryHigh` of 6 GiB. Crossing that line puts the process into
/// kernel direct-reclaim throttling (measured on the wedged node: PSI memory
/// `full avg300 = 97.17`, 1.93M throttle events, `pgscan_kswapd = 0` so ALL
/// reclaim is synchronous) — which makes it apply blocks *slower*, which makes
/// it fall *further* behind, which buffers *more*. That is the death spiral
/// that wedged sigil-node three times.
///
/// The distance bound matters as much as the count bound: 200k heights of slack
/// is still ~560 MiB. Blocks beyond the horizon are dropped, not buffered — the
/// backfill requester will re-fetch them in order as the tip advances, which is
/// the mechanism that is supposed to close a gap anyway.
pub const PENDING_MAX_ENTRIES: usize = 32_768;
pub const PENDING_MAX_AHEAD: u64 = 32_768;

/// Buffer a gossiped/backfilled block by height, bounded in BOTH directions.
/// Returns true if it was retained. Every `pending` insert must go through here.
pub fn pending_insert(
    pending: &mut BTreeMap<u64, Block>,
    tip_height: u64,
    height: u64,
    block: Block,
) -> bool {
    // Already settled — never buffer the past.
    if height < tip_height {
        return false;
    }
    // Beyond the horizon: dropping is correct. Ordered backfill will supply it.
    if height.saturating_sub(tip_height) > PENDING_MAX_AHEAD {
        return false;
    }
    if pending.len() >= PENDING_MAX_ENTRIES && !pending.contains_key(&height) {
        // Full: keep the CLOSEST-to-tip work (that is what unblocks the applier)
        // and evict the farthest-ahead entry, but only if the newcomer is nearer.
        let farthest = match pending.keys().next_back().copied() {
            Some(k) => k,
            None => return false,
        };
        if height >= farthest {
            return false;
        }
        pending.remove(&farthest);
    }
    pending.entry(height).or_insert(block);
    true
}

/// Bounded insert into the SIGIL_DAG=1 body store. On overflow the
/// lowest-height resident body is dropped first (approximation of the design's
/// "lowest-height off-spine first" — the linear path's 200k `pending` cap is
/// the precedent). O(n) scan only on overflow.
pub fn dag_store_body(
    dag_bodies: &mut HashMap<BlockHash, Block>,
    cap: usize,
    hash: BlockHash,
    block: Block,
) {
    if dag_bodies.len() >= cap && !dag_bodies.contains_key(&hash) {
        if let Some(evict) = dag_bodies
            .iter()
            .min_by_key(|(h, b)| (b.header.height, **h))
            .map(|(h, _)| *h)
        {
            dag_bodies.remove(&evict);
        }
    }
    dag_bodies.insert(hash, block);
}

/// Bound on `mint_hash_to_tx_hashes` — same order of magnitude as `dag_bodies`'
/// default cap (32,768). In practice this map stays tiny (only OUR OWN
/// candidates that carried at least one pending send get an entry at all,
/// and every entry is removed the moment its candidate is either confirmed
/// or falls out of `dag_bodies`), so this cap is a backstop against a
/// pathological run of never-confirmed candidates, not a normal-path limit.
pub const MINT_HASH_TRACKING_CAP: usize = 32_768;

/// Drop tracking entries for candidates `dag_bodies` no longer even holds —
/// those candidates are long orphaned (evicted below the finalized window),
/// so they will never be looked up by `dag_drain_apply` again. Their tx
/// hashes are NOT lost: they were never removed from `SendBridge`'s pending
/// map, so they simply keep riding along on every future mint attempt via
/// `snapshot_for_mint` until one lands.
pub fn prune_mint_hash_tracking(
    mint_hash_to_tx_hashes: &mut HashMap<BlockHash, Vec<[u8; 32]>>,
    dag_bodies: &HashMap<BlockHash, Block>,
) {
    mint_hash_to_tx_hashes.retain(|h, _| dag_bodies.contains_key(h));
}

/// How many recent, already-committed blocks the QTFT topology commitment
/// windows over. Bounded, per SIGIL_QTFT_TOPOLOGY_v0.md's "the tractable
/// core uses a CHEAP invariant over a bounded window" design — a full-DAG
/// invariant is not what's computed here.
pub const TOPOLOGY_COMMITMENT_WINDOW: u64 = 32;

/// Compute the QTFT topology commitment for the block about to be minted at
/// `next_height`: the exact Alexander polynomial (`flux_topology`) of the
/// braid word (`sigil_dagknight::present::braid_word`, QTFT-1's own
/// documented one-line bridge) over the bounded window of already-resident
/// history `[next_height - TOPOLOGY_COMMITMENT_WINDOW, next_height - 1]`,
/// hashed with domain separation. `None` when there's no live `Braid`
/// (linear/non-DAG mode, `SIGIL_DAG=0`) or no prior window exists yet
/// (genesis, `next_height == 0`).
///
/// **Honest note on what this actually computes on SIGIL today:** with a
/// single producer, every block in every window shares one producer id, so
/// `braid_word` always yields the empty word on 1 strand — the Alexander
/// polynomial of the unknot, a fixed, non-informative value. This becomes a
/// real, block-to-block-varying invariant the moment a second real producer
/// joins the mesh. It is computed and committed now anyway so the field's
/// history is tamper-evident (part of `signing_bytes()`) from the moment it
/// starts appearing, not retroactively once it becomes interesting.
pub fn compute_topology_commitment(braid: Option<&Braid>, next_height: u64) -> Option<[u8; 32]> {
    let braid = braid?;
    if next_height == 0 {
        return None;
    }
    let to_height = next_height - 1;
    let from_height = to_height.saturating_sub(TOPOLOGY_COMMITMENT_WINDOW.saturating_sub(1));
    let bp = braid.braid_word(from_height, to_height);
    if !window_is_complete(&bp, from_height, to_height) {
        // 2026-08-20: real incident — a node whose window was populated via
        // bulk backfill (not one-at-a-time live gossip) can have a window
        // that's in-range but not fully resident (eviction racing catch-up).
        // Committing over a partial window would silently produce a
        // different value than a node with the full window — refuse rather
        // than emit a value nobody else could reproduce. See
        // `verify_topology_on_receipt` for the matching receiver-side guard.
        return None;
    }
    let bw = flux_topology::BraidWord { strands: bp.strands, gens: bp.word.clone() };
    let delta = flux_topology::alexander_poly(&bw);
    topology_commit_hash(&delta, bp.strands, &bp.word, &bp.producers)
}

/// Is every height in `[from_height, to_height]` actually resident in the
/// braid state `bp` was extracted from? `braid_word` silently skips
/// non-resident blocks (present.rs's own module doc) rather than erroring,
/// so an incomplete window looks exactly like a valid, smaller one unless
/// checked explicitly — this is that check.
pub fn window_is_complete(bp: &sigil_dagknight::present::BraidPresentation, from_height: u64, to_height: u64) -> bool {
    if from_height > to_height {
        return true; // empty window, vacuously complete
    }
    let expected = (to_height - from_height + 1) as usize;
    bp.heights_resident == expected
}

/// 2026-08-20: the Alexander polynomial ALONE is not a collision-resistant
/// commitment — it's a well-established fact in knot theory that distinct
/// braids/knots can share the same Δ(t) (the Kinoshita–Terasaka knot and the
/// unknot both have Δ=1; Alexander is a coarse invariant, not designed to
/// resist an adversary hunting for a collision the way BLAKE3 is). A
/// dishonest producer wouldn't need to break BLAKE3 to hide a divergent
/// window — only find some OTHER window that happens to share the same Δ.
///
/// Fix: bind the commitment to the EXACT canonical braid presentation
/// (strands, word, producer ranking), not just its polynomial image.
/// `braid_word` is already proven deterministic regardless of arrival order
/// (present.rs's `braid_word_deterministic_across_arrival_orders` test), so
/// hashing it directly costs nothing in determinism — the Alexander
/// polynomial is kept alongside it for what it actually adds (a real
/// topological classification of the window, useful for future analysis).
pub fn topology_commit_hash(
    delta: &flux_topology::LaurentPoly,
    strands: u32,
    word: &[i32],
    producers: &[[u8; 32]],
) -> Option<[u8; 32]> {
    let poly_bytes = serde_json::to_vec(delta).ok()?;
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/QTFT/TOPOLOGY/V1");
    h.update(b"|poly|");
    h.update(&poly_bytes);
    h.update(b"|strands|");
    h.update(&strands.to_le_bytes());
    h.update(b"|word|");
    for g in word {
        h.update(&g.to_le_bytes());
    }
    h.update(b"|producers|");
    for p in producers {
        h.update(p);
    }
    Some(*h.finalize().as_bytes())
}
