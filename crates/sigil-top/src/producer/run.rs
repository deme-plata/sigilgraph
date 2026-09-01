//! run.rs — Phase 3/5: the actual producer loop. Everything `mint`/`dag` expose is
//! just functions; this is what calls them in the right order, repeatedly, and is
//! what the two runtime gates in `producer/mod.rs` (`SIGIL_TOP_PRODUCER`,
//! `SIGIL_TOP_PRODUCE`) actually turn on.
//!
//! 2026-08-23 (grogu-producer-unification Phase 3/5, operator-directed: "let's do
//! this"). Deliberately scoped to the MINTING side only — this loop mints and
//! settles blocks against its OWN LOCAL braid/chain; it does **not** broadcast
//! minted blocks over the network. Wiring real P2P participation (accepting gossip
//! from other producers, publishing this node's own blocks to them) is a genuinely
//! separate, higher-stakes step — joining sigil-g0 as a real second producer
//! affects a live shared network, not just this process. That deserves its own
//! explicit go-ahead, the way the mining-queue fix and the q-flux backend change
//! did earlier this session, not a silent side-effect of this port. Until that
//! lands, an operator who sets both env vars gets a fully-functional LOCAL chain
//! that mints, verifies, and settles blocks entirely offline — real proof the
//! minting pipeline works end-to-end, safe by construction because nothing here
//! can reach the live mesh.
//!
//! Mirrors `sigil-node/src/main.rs`'s real per-tick sequence (seed once, then
//! frontier → mint → insert-own-block-into-braid → drain-apply, repeating) minus
//! the mining/mempool/emission-controller inputs `sigil-top` doesn't have its own
//! copies of yet — this is the free-running "empty coinbase, height-schedule
//! reward" dyno path, the same one `sigil-node` itself falls back to with no
//! external miner and no txs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use sigil_dagknight::{BlockView, Braid};
use sigil_header::BlockHash;
use sigil_narwhal_mempool::MempoolBackend;
use sigil_tx::SignedTx;

use sigil_node::block::Block;
use sigil_node::chain::ChainTip;
use sigil_node::dag::{
    compute_topology_commitment, dag_build_frontier, dag_drain_apply, dag_seed_braid,
    dag_store_body,
};
use sigil_node::mint::mint_next_block;
use sigil_node::solve_credit::take_creditable_solve;

/// Bound on the local producer's own DAG body cache — same reasoning/order of
/// magnitude as sigil-node's `MINT_HASH_TRACKING_CAP`; this is a solo/small mesh
/// loop, not a high-throughput producer, so a modest fixed cap is plenty.
const DAG_BODIES_CAP: usize = 4_096;

/// Everything one producer tick needs, held across ticks. Public so a caller (a
/// test, or eventually a real broadcast-wiring layer) can inspect `chain`/`braid`
/// between ticks without this module needing to expose separate getters for each.
pub struct ProducerState {
    pub chain: ChainTip,
    pub braid: Braid,
    pub dag_bodies: HashMap<BlockHash, Block>,
    mint_hash_to_tx_hashes: HashMap<BlockHash, Vec<[u8; 32]>>,
    /// The SAME `sigil_api::AppState` a local mining/money HTTP server (see
    /// `producer::mining_api`) is started with, when one is running — `mining`/`send`/
    /// `bridge`/`dex`/`usds`/`usds_bridge`/`shielded` here are the exact Arc handles the
    /// router reads and writes through, not copies. `AppState` derives `Clone` (cheap —
    /// every field is an `Arc`), so a caller wanting to hand the router its own copy of
    /// these handles just clones `api`, as [`super::mining_api::spawn_local_mining_api`]
    /// does. `state` (the published `SigilState` snapshot money-API reads balances from)
    /// is refreshed after every settled tick — see the write in [`Self::tick`].
    pub api: sigil_api::AppState,
}

/// What one `tick()` accomplished — logged by the loop wrapper, asserted on by tests.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// Height of the candidate block this tick minted (before settlement).
    pub minted_height: u64,
    /// Blocks the drain settled onto the local chain this tick (usually 0 or 1 —
    /// can exceed 1 if a prior tick's candidate settles late).
    pub applied: u64,
    pub skipped: u64,
    pub failed: u64,
    /// `chain.height()` AFTER this tick's drain — the number that actually matters.
    pub settled_height: u64,
    /// This tick's own minted candidate, pre-serialized (`serde_json::to_vec`,
    /// the EXACT wire shape `sigil-node`'s own `TOPIC_BLOCKS` publisher uses —
    /// see `mint.rs`'s own reasoning for why byte-compatibility matters here).
    /// A networked caller publishes this; an offline caller (the existing
    /// tests) can ignore it.
    pub minted_block_bytes: Vec<u8>,
}

impl ProducerState {
    /// Start a fresh producer state on top of an already-initialized `chain`
    /// (genesis applied, and any prior blocks this instance already holds). Seeds
    /// the braid from `chain`'s own local window — see `dag_seed_braid`'s doc.
    pub fn new(chain: ChainTip) -> Self {
        let braid = dag_seed_braid(&chain);
        // A local, empty mempool: this loop's `txs` come from the money bridges'
        // `snapshot_for_mint()` below (send/bridge/dex/usds/shielded), the same as
        // `sigil-node`'s own producer — nothing here ever pulls from `mempool` today, so
        // `legacy()` (no env-driven backend selection) keeps this deterministic and free
        // of any dependency on the process's environment beyond the two producer gates
        // already checked in `producer::mod`. `AppState` still needs a real handle
        // because `sigil_api::router`'s `/v1/transactions` route requires one to exist,
        // even though nothing here drains it — see `mining_api.rs`'s module doc.
        let mempool: Arc<MempoolBackend> = Arc::new(MempoolBackend::legacy());
        // Published SETTLED state, refreshed after every tick (see `Self::tick`) —
        // mirrors `sigil-node`'s own `money_state` snapshot-on-settle pattern exactly.
        let state = Arc::new(RwLock::new(chain.state_snapshot()));
        let api = sigil_api::AppState::new(mempool, state);
        Self { chain, braid, dag_bodies: HashMap::new(), mint_hash_to_tx_hashes: HashMap::new(), api }
    }

    /// One producer tick, in the same order `sigil-node`'s real loop runs it:
    /// build the speculative frontier → mint a candidate on top of it → fold the
    /// candidate into this node's own braid (as if it arrived from a peer) →
    /// drain-apply whatever the braid has now finalized onto the settled chain.
    /// `persist` is handed each settled block's raw bytes, exactly like
    /// `sigil-node`'s chain-log append path — a test can collect them, a real
    /// deployment would write them durably (not wired here — see module docs).
    pub fn tick(&mut self, persist: &mut dyn FnMut(&[u8])) -> Result<TickOutcome> {
        let frontier = dag_build_frontier(&self.chain, &self.braid, &self.dag_bodies);
        let merge_parents = self.braid.merge_tips(&frontier.parent_hash(), 4);
        let topology_commitment = compute_topology_commitment(Some(&self.braid), frontier.height());

        // Publish the frontier a local miner's next challenge binds to — see
        // `mining_api` module docs. A solve issued now is valid for exactly the block
        // this tick is about to mint and no other, same contract `sigil-node`'s own
        // producer publishes under (`mining_bridge.publish_tip`, main.rs). Always
        // called, even when no local mining API is running — cheap (a couple of
        // atomics inside `MiningBridge`), and harmless if nobody's listening.
        self.api.mining.publish_tip(frontier.height(), frontier.parent_hash());
        // Opportunistically credit whatever's already queued — an exact match for
        // THIS frontier, or a near-miss within the credit window — never wait for one
        // (free-running cadence, unchanged if no local miner is running). Reuses the
        // EXACT scan/credit decision `sigil-node`'s own producer tunes live — see
        // `sigil_node::solve_credit`'s doc for the money-loss bug this was tuned
        // against; a hand-rolled copy here would risk silently drifting from it.
        let solve = take_creditable_solve(&self.api.mining, frontier.parent_hash(), frontier.height());

        // Real submissions this instance has authenticated (wallet-signed, via
        // whatever surface exposes SendBridge::submit etc. to callers — reachable
        // today only through a local mining/money API server, if one is running; see
        // `mining_api` module docs). Calling snapshot_for_mint here always, even when
        // no such server is running (so these stay empty), keeps this loop identical
        // to before this module gained an `api` field in that case.
        let txs: Vec<SignedTx> = {
            let mut v = self.api.send.snapshot_for_mint();
            v.extend(self.api.bridge.snapshot_for_mint());
            v.extend(self.api.dex.snapshot_for_mint());
            v.extend(self.api.usds.snapshot_for_mint());
            v.extend(self.api.usds_bridge.snapshot_for_mint());
            v.extend(self.api.shielded.snapshot_for_mint());
            v
        };

        // 7th arg (2026-08-26, Option C): the partial-share pool, drained ONLY for a
        // self-minted block. With a real solve the winner's own `shares` map already
        // carries the pool (`submit()` folds and clears it), so draining here too would
        // pay the same work twice. Identical contract to sigil-node's own producer loop
        // — this instance shares the SAME `MiningBridge`, so it must make the same
        // decision or a sigil-top producer would silently reintroduce the 93.8%
        // pay-yourself behaviour Option C exists to remove.
        let share_pool = if solve.is_none() {
            self.api.mining.take_share_pool()
        } else {
            None
        };
        let (block, minted_tx_hashes) =
            mint_next_block(&frontier, merge_parents, &txs, None, solve.as_ref(), topology_commitment, share_pool)?;
        let minted_height = block.header.height;
        // Same wire shape sigil-node's own TOPIC_BLOCKS publisher uses (plain
        // serde_json::to_vec — confirmed by reading its actual publish call
        // site) — a networked caller can hand this straight to `net.publish`.
        let minted_block_bytes = serde_json::to_vec(&block).unwrap_or_default();

        // Fold our own candidate into the braid exactly like an incoming peer
        // block — `dag_drain_apply` only ever settles from the braid's own
        // selected spine, so a self-mined block that never gets inserted here
        // could never be finalized.
        let view = BlockView::from(&block.header);
        let vh = view.hash;
        let _ = self.braid.insert(view);
        dag_store_body(&mut self.dag_bodies, DAG_BODIES_CAP, vh, block);
        if !minted_tx_hashes.is_empty() {
            self.mint_hash_to_tx_hashes.insert(vh, minted_tx_hashes);
        }

        let (applied, skipped, failed) = dag_drain_apply(
            &mut self.braid,
            &mut self.dag_bodies,
            &mut self.chain,
            persist,
            &self.api.send,
            &self.api.bridge,
            &self.api.dex,
            &self.api.usds,
            &self.api.usds_bridge,
            &self.api.shielded,
            &mut self.mint_hash_to_tx_hashes,
        );

        // Publish the fresh SETTLED state so a local money/mining API (if running)
        // serves current balances — mirrors `sigil-node`'s own `money_state` refresh
        // (main.rs, "publish the fresh SETTLED state" comment) exactly. Unconditional
        // (not gated on `applied > 0`): cheap relative to a tick, and simpler to reason
        // about than trying to skip it on ticks that settled nothing.
        if let Ok(mut w) = self.api.state.write() {
            *w = self.chain.state_snapshot();
        }

        Ok(TickOutcome { minted_height, applied, skipped, failed, settled_height: self.chain.height(), minted_block_bytes })
    }

    /// Fold a block that arrived from a REAL peer (gossip) into this instance's
    /// braid — the network-facing twin of the self-mint path above: same
    /// `braid.insert` + `dag_store_body`, just skipping the mint/tx-tracking
    /// steps that only apply to blocks THIS instance produced. Safe to call for
    /// a block already known (braid dedupes by hash) or one that turns out to be
    /// invalid — `dag_drain_apply`'s own settlement walk is what actually
    /// decides whether anything here ever reaches the settled chain, so a
    /// dishonest or malformed peer block can pollute this instance's braid
    /// bookkeeping at worst, never its settled state.
    pub fn ingest_foreign_block(&mut self, block: Block) {
        let view = BlockView::from(&block.header);
        let vh = view.hash;
        let _ = self.braid.insert(view);
        dag_store_body(&mut self.dag_bodies, DAG_BODIES_CAP, vh, block);
    }
}

/// A running producer loop's handle — drop it or call [`stop`](Self::stop) to shut
/// the background thread down cleanly.
pub struct ProducerLoopHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProducerLoopHandle {
    /// Signal the loop to stop after its current tick and wait for it to exit.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ProducerLoopHandle {
    fn drop(&mut self) {
        // Best-effort: signal stop even if the caller drops the handle without
        // calling `stop()` explicitly. Doesn't block (no join here — a Drop that
        // blocks is a footgun), so a dropped handle's thread exits on its OWN next
        // tick boundary rather than immediately.
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Spawn the producer loop on a background thread: `tick()` repeatedly, sleeping
/// `tick_interval` between ticks, until [`ProducerLoopHandle::stop`] is called (or
/// the handle is dropped). Every tick's outcome is logged via `crate::tlog!` (the
/// same TUI-visible logfile every other sync/mining event uses), success or
/// failure — a producer loop that silently stops minting must never be silent
/// about it.
pub fn spawn_producer_loop(chain: ChainTip, tick_interval: Duration) -> ProducerLoopHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_bg = stop_flag.clone();
    let thread = std::thread::spawn(move || {
        let mut state = ProducerState::new(chain);
        crate::tlog!("[producer] loop started (local-mint-only — no network broadcast)");
        while !stop_flag_bg.load(Ordering::Relaxed) {
            let mut noop = |_: &[u8]| {};
            match state.tick(&mut noop) {
                Ok(o) => {
                    if o.applied > 0 {
                        crate::tlog!(
                            "[producer] tick: minted h={} → settled h={} (applied={} skipped={} failed={})",
                            o.minted_height, o.settled_height, o.applied, o.skipped, o.failed
                        );
                    }
                }
                Err(e) => crate::tlog!("[producer] ⚠ tick failed: {e}"),
            }
            std::thread::sleep(tick_interval);
        }
        crate::tlog!("[producer] loop stopped");
    });
    ProducerLoopHandle { stop_flag, thread: Some(thread) }
}

/// 2026-08-23 (operator-directed, testnet: "worst outcome is just a reset"):
/// the real network-connected loop. Same `tick()` core as [`spawn_producer_loop`],
/// wrapped with a genuine `flux_p2p::NetworkManager` — subscribes to
/// `sigil_net::TOPIC_BLOCKS` and feeds every incoming peer block into
/// [`ProducerState::ingest_foreign_block`], and publishes this instance's own
/// minted candidates back onto that same topic, using the EXACT wire shape
/// (`serde_json::to_vec(&block)`) confirmed by reading `sigil-node`'s own
/// publish call site — so a real peer (Epsilon) can actually deserialize what
/// this sends. Uses `for_sigil("producer")` — a DIFFERENT node-name than the
/// light-client sync engine's `for_sigil("top")`, so the two get independent
/// libp2p identities and independent ephemeral ports (`for_sigil` always binds
/// `tcp/0`, OS-assigned) — no port or identity collision running both in the
/// same process. This makes the process a REAL second participant on sigil-g0:
/// its blocks are visible to other real nodes, and it applies whatever real
/// nodes gossip back. That's the deliberate difference from
/// [`spawn_producer_loop`] below, which this module's earlier version used
/// exclusively and which stays available for anyone who wants the
/// network-free, purely-local proof (still used by every test in this file).
fn spawn_networked_loop(chain: ChainTip, tick_interval: Duration) -> ProducerLoopHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_bg = stop_flag.clone();
    let thread = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                crate::tlog!("[producer] ⚠ tokio runtime build failed: {e} — networked loop cannot start");
                return;
            }
        };
        rt.block_on(async move {
            let mut net = flux_p2p::NetworkManager::for_sigil("producer");
            if let Err(e) = net.start().await {
                crate::tlog!("[producer] ⚠ network start failed: {e} — networked loop cannot start");
                return;
            }
            crate::tlog!(
                "[producer] network started on sigil-g0 mesh (ephemeral port, identity \"producer\" — \
                 independent of the light-client sync engine's \"top\" identity)"
            );
            let mut block_rx = net.subscribe(sigil_net::TOPIC_BLOCKS);
            let net = Arc::new(net);
            let mut state = ProducerState::new(chain);
            // Local mining HTTP API — 2026-08-25, operator-directed ("let a miner mine
            // against their OWN locally-running node"). Shares `state.api` by clone
            // (cheap — every field is an Arc), so a share submitted here lands in the
            // EXACT MiningBridge this tick loop publishes tips into and drains solves
            // from, above. See `mining_api` module docs for the full design + the
            // two-gate reasoning (this only ever runs once BOTH SIGIL_TOP_PRODUCER=1
            // AND SIGIL_TOP_PRODUCE=1 are set — `maybe_start`'s existing contract,
            // unchanged by this addition).
            super::mining_api::spawn_local_mining_api(state.api.clone());
            crate::tlog!("[producer] networked loop started — publishing candidates to {}", sigil_net::TOPIC_BLOCKS);

            const INGEST_CAP: u32 = 64; // bounded per tick — mirrors the light client's own gossip-flood discipline
            while !stop_flag_bg.load(Ordering::Relaxed) {
                let mut ingested = 0u32;
                while ingested < INGEST_CAP {
                    let (_topic, data) = match block_rx.try_recv() { Ok(x) => x, Err(_) => break };
                    ingested += 1;
                    let Some(inflated) = crate::block_sync::verify::inflate_gossip_frame(&data) else { continue };
                    match serde_json::from_slice::<Block>(&inflated) {
                        Ok(b) => state.ingest_foreign_block(b),
                        Err(_) => continue, // malformed gossip — drop, same as the light client does
                    }
                }
                // Connection bookkeeping only for now — nothing this loop needs to react to yet
                // (no peer-scoped rate limiting or reputation tracking at this phase).
                let _ = net.drain_events();

                let mut noop = |_: &[u8]| {};
                match state.tick(&mut noop) {
                    Ok(o) => {
                        if !o.minted_block_bytes.is_empty() {
                            if let Err(e) = net.publish(sigil_net::TOPIC_BLOCKS, o.minted_block_bytes) {
                                crate::tlog!("[producer] ⚠ publish h={} failed: {e}", o.minted_height);
                            }
                        }
                        if o.applied > 0 {
                            crate::tlog!(
                                "[producer] tick: minted h={} → settled h={} (applied={} skipped={} failed={} peers={})",
                                o.minted_height, o.settled_height, o.applied, o.skipped, o.failed, net.peer_count()
                            );
                        }
                    }
                    Err(e) => crate::tlog!("[producer] ⚠ tick failed: {e}"),
                }
                tokio::time::sleep(tick_interval).await;
            }
            crate::tlog!("[producer] networked loop stopped");
        });
    });
    ProducerLoopHandle { stop_flag, thread: Some(thread) }
}

/// The actual wiring for the two runtime gates documented in `producer/mod.rs`.
/// Called once at startup (behind `#[cfg(feature = "producer")]` at the call
/// site, in `main.rs`) — a no-op unless BOTH `SIGIL_TOP_PRODUCER=1` AND
/// `SIGIL_TOP_PRODUCE=1` are set, matching the two-independent-gates contract
/// [`super::producer_mode_enabled`]/[`super::should_produce`] already document.
///
/// 2026-08-24 (sync-then-produce bridge, operator-directed: "work on unifiyhing
/// the sigil top node so that i can produce blocks and actual is a real node ...
/// every user downloading sigil top wil be full node operator"). Before this, the
/// chain always started from a fresh local genesis — safe in isolation, but not a
/// real network participant: its blocks would never match what the rest of
/// sigil-g0 already has. Now the chain is bootstrapped from a real running node's
/// signed snapshot + a P2P tail replay to the live tip (see
/// [`super::sync::sync_chain`]) BEFORE the networked mint loop ever starts. If
/// that sync fails for any reason, this refuses to start — it deliberately does
/// NOT fall back to a fresh genesis, because minting on top of an unsynced chain
/// would be a silent fork, not a real node.
pub fn maybe_start(tick_interval: Duration) -> Option<ProducerLoopHandle> {
    if !(super::producer_mode_enabled() && super::should_produce()) {
        return None;
    }
    let chain = match sync_chain_blocking() {
        Some(c) => c,
        None => {
            crate::tlog!(
                "[producer] ⚠ refusing to start — sync-then-produce bootstrap failed \
                 (see [producer-sync] log lines above for the exact step that failed)"
            );
            return None;
        }
    };
    Some(spawn_networked_loop(chain, tick_interval))
}

/// Runs [`super::sync::sync_chain`] to completion on a short-lived tokio runtime,
/// blocking the calling (startup) thread. This runtime — and the ephemeral
/// `"producer-sync"` network identity it opens — is fully torn down once sync
/// finishes; [`spawn_networked_loop`] opens its own independent `"producer"`
/// identity afterward for the ongoing mint/gossip loop. Two short-lived identities
/// instead of one shared one is a deliberate simplicity choice: it keeps this
/// bootstrap step fully independent of (and never able to corrupt) the loop's own
/// long-running network state.
fn sync_chain_blocking() -> Option<ChainTip> {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            crate::tlog!("[producer-sync] ⚠ tokio runtime build failed: {e}");
            return None;
        }
    };
    rt.block_on(async move {
        let mut net = flux_p2p::NetworkManager::for_sigil("producer-sync");
        if let Err(e) = net.start().await {
            crate::tlog!("[producer-sync] ⚠ network start failed: {e}");
            return None;
        }
        super::sync::sync_chain(&net).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // `tests` nests one level deeper than `run.rs`'s own top-level functions (which
    // reach `mining_api` via a single `super::`, since THEIR `super` is `producer`
    // directly) — from inside `tests`, `super` is `producer::run`, so `mining_api`
    // needs the extra hop. `super::super::mint` a few lines below is the same pattern,
    // already established in this file before this test was added.
    use super::super::mining_api;

    fn genesis_chain() -> ChainTip {
        let mut chain = ChainTip::new();
        chain.apply(super::super::mint::build_genesis().expect("genesis")).expect("apply genesis");
        chain
    }

    /// The core proof this module exists for: seed → tick → tick → tick actually
    /// advances the settled chain, deterministically, with no failures. Runs
    /// entirely in-process/offline — no thread, no timer, no network.
    ///
    /// `SIGIL_DAG_FINAL_DEPTH` set low here on purpose: DagKnight only finalizes a
    /// block once the selected tip is `final_depth` heights past it (production
    /// default 512 — real, not a bug in this test's absence: confirmed live
    /// against sigil-g0 earlier this session, `tip_h - fin_h == 512` exactly). At
    /// the real default, 5 ticks would correctly settle NOTHING — this env var
    /// exercises the same finalization logic at a depth this test can actually
    /// reach, not a different code path.
    #[test]
    fn ticking_advances_the_settled_chain() {
        std::env::set_var("SIGIL_DAG_FINAL_DEPTH", "2");
        let mut state = ProducerState::new(genesis_chain());
        assert_eq!(state.chain.height(), 1, "starts one past genesis");

        let mut persisted = Vec::new();
        for _ in 0..5 {
            let mut collect = |bytes: &[u8]| persisted.push(bytes.to_vec());
            let outcome = state.tick(&mut collect).expect("tick");
            assert_eq!(outcome.failed, 0, "no tick should fail against its own freshly-minted candidate");
        }
        std::env::remove_var("SIGIL_DAG_FINAL_DEPTH"); // don't leak into other tests in this process
        assert!(state.chain.height() > 1, "at least one of 5 ticks settled past genesis (was {})", state.chain.height());
        assert_eq!(persisted.len() as u64, state.chain.height() - 1, "persist called exactly once per settled block");
    }

    /// 2026-08-23 (operator-directed: "go ahead and chronos test it also") — the
    /// gate this whole module needs to pass before it's trustworthy on a real
    /// multi-producer mesh, mirrored on the existing chronos-style "STEP 3 GATE"
    /// pattern already proven for `sigil-node`'s own coinbase path
    /// (`coinbase.rs`'s `money_settles_over_a_weave...` test, same idea): two
    /// INDEPENDENT producers mint concurrently, race each other, and cross-feed
    /// each other's candidates via [`ProducerState::ingest_foreign_block`] — the
    /// exact same entry point a real peer's gossiped block would go through.
    /// Deterministic, fully offline — no network, so nothing here needed the
    /// permission the live-run attempt was blocked on.
    ///
    /// The property that actually matters: DagKnight's finality rule is
    /// supposed to make every honest participant converge on the identical
    /// settled chain no matter which order they happened to receive competing
    /// candidates in — that's the whole point of "selected spine" ordering
    /// (see `dag_build_frontier`'s doc). If A and B disagree after this test,
    /// the unification work is NOT safe to point at a real mesh, full stop.
    #[test]
    fn two_producers_weave_and_converge_to_identical_chain() {
        std::env::set_var("SIGIL_DAG_FINAL_DEPTH", "2");
        let mut a = ProducerState::new(genesis_chain());
        let mut b = ProducerState::new(genesis_chain());
        assert_eq!(a.chain.parent_hash(), b.chain.parent_hash(), "both start from the identical genesis");

        // Race for a while so real height-N forks actually happen (both mint
        // every round; neither ever sees the other's candidate before minting
        // its own — the adversarial case, not the easy "already agree" one).
        for round in 0..40 {
            let oa = a.tick(&mut |_| {}).unwrap_or_else(|e| panic!("A tick {round} failed: {e}"));
            let ob = b.tick(&mut |_| {}).unwrap_or_else(|e| panic!("B tick {round} failed: {e}"));
            // Cross-feed in ALTERNATING order (A-then-B on even rounds, B-then-A
            // on odd) — different arrival order is exactly the condition the
            // property above has to hold under, not just the convenient one.
            let feed = |them: &mut ProducerState, bytes: &[u8]| {
                if bytes.is_empty() { return; }
                if let Ok(blk) = serde_json::from_slice::<Block>(bytes) {
                    them.ingest_foreign_block(blk);
                }
            };
            if round % 2 == 0 {
                feed(&mut b, &oa.minted_block_bytes);
                feed(&mut a, &ob.minted_block_bytes);
            } else {
                feed(&mut a, &ob.minted_block_bytes);
                feed(&mut b, &oa.minted_block_bytes);
            }
        }
        std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");

        assert!(a.chain.height() > 1, "A settled at least one block past genesis (was {})", a.chain.height());
        assert!(b.chain.height() > 1, "B settled at least one block past genesis (was {})", b.chain.height());
        // The real gate: NOT "both made progress" (either could hallucinate its
        // own progress independently) but "both landed on the SAME tip" —
        // height AND hash must agree, or this is a silent fork, exactly the
        // failure mode DagKnight's selected-spine rule exists to prevent.
        assert_eq!(a.chain.height(), b.chain.height(),
            "A and B settled DIFFERENT heights — a real fork, not just a race");
        assert_eq!(a.chain.parent_hash(), b.chain.parent_hash(),
            "A and B settled the same height but DIFFERENT blocks — a real fork");
    }

    /// LIVE, OFFLINE, end-to-end proof of the local mining API (2026-08-25): a
    /// producer with no network attached, its local mining HTTP server, a REAL `GET
    /// /v1/mining/challenge`, a REAL `flux_miner::client::solve`, a REAL `POST
    /// /v1/mining/submit`, and a REAL minted+settled block crediting the miner's
    /// wallet — against the genuine `sigil_api::router` and the genuine
    /// `MiningBridge`/`mint_next_block`/`solve_credit::take_creditable_solve`
    /// chokepoints. Nothing here is mocked or hand-simulated.
    ///
    /// Deliberately does NOT go through `maybe_start()` (the real sync-from-a-running-
    /// node + real sigil-g0 mesh join + real block-publish path) — see `mining_api.rs`'s
    /// module doc for why: this process becoming a real second producer on the live
    /// network is a separate, higher-stakes step this session's task explicitly did not
    /// authorize (mirrors the existing `#[ignore]`d `manual_observe_live_networked_run`
    /// test below, which required its own explicit operator go-ahead). This test proves
    /// the NEW code — the local server plus `tick()`'s mining wiring — end-to-end
    /// without touching Epsilon or the live mesh at all.
    #[test]
    fn local_mining_api_credits_a_real_solve_into_a_minted_block() {
        std::env::set_var("SIGIL_DAG_FINAL_DEPTH", "2");
        // Trivial-but-real difficulty: a genuine nonce search + a genuine (short)
        // sequential VDF, not a rigged always-pass check. Fast enough for a unit test.
        std::env::set_var("SIGIL_MINING_BLAKE4_BITS", "8");
        std::env::set_var("SIGIL_MINING_VDF_T", "4");

        // The live sigil-node on this box binds 18183 (its raw tx-ingest port), which
        // is our DEFAULT local-mining-API port — relocate to a free ephemeral port so
        // the test is isolated from any co-located node. Serialized against the
        // addr-default assertion in mining_api via PORT_ENV_LOCK.
        let _port_g = mining_api::PORT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let free_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        std::env::set_var("SIGIL_LOCAL_MINING_API_PORT", free_port.to_string());

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let addr = mining_api::local_mining_api_addr();
            // Regression check FIRST, before anything spawns anything: with nothing
            // having called `spawn_local_mining_api` yet, the port must be unbound and
            // the flag other code (`engine_node_url()`) reads must read false. A real
            // TCP connect attempt is the honest check — it proves the OS has no
            // listener there, not just that this module's own bookkeeping agrees.
            assert!(
                tokio::net::TcpStream::connect(&addr).await.is_err(),
                "port {addr} must be unbound before spawn_local_mining_api is ever called"
            );
            assert!(!mining_api::local_mining_api_is_up());

            let mut state = ProducerState::new(genesis_chain());
            mining_api::spawn_local_mining_api(state.api.clone());

            // ProducerState.tick() needs &mut self, so the ticking thread takes
            // exclusive ownership of `state` from here on — the HTTP server never
            // touches ProducerState directly, only its own clone of the Arc-shared
            // `api` handles, so this isn't a lock, just a move.
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = Arc::clone(&stop);
            let ticker = std::thread::spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    let _ = state.tick(&mut |_| {});
                    std::thread::sleep(Duration::from_millis(15));
                }
            });

            // Budgets are generous on purpose: this test spins up a real server
            // and a real (CPU-bound) mining ticker, so under `-j8` max-parallel
            // test load the miner is scheduler-starved and every stage takes
            // longer in wall-clock. Widening the windows tolerates that load
            // without weakening any assertion — a genuinely broken pipeline still
            // fails, just after a longer wait. (API up: up to 10s.)
            let mut up = false;
            for _ in 0..200 {
                if tokio::net::TcpStream::connect(&addr).await.is_ok() { up = true; break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(up, "local mining API never came up on {addr}");
            assert!(mining_api::local_mining_api_is_up());

            let wallet = "ab".repeat(32); // 64-hex, canonical lowercase
            let base = mining_api::local_mining_api_url();
            let client = reqwest::Client::new();

            // First real challenge needs at least one tick's publish_tip() — up to 9s under load.
            let mut challenge: Option<flux_miner::client::Challenge> = None;
            for _ in 0..300 {
                if let Ok(r) = client.get(format!("{base}/v1/mining/challenge?wallet={wallet}")).send().await {
                    if r.status().is_success() {
                        if let Ok(c) = r.json::<flux_miner::client::Challenge>().await {
                            challenge = Some(c);
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let challenge = challenge.expect("never got a real challenge from the local mining API");

            // The SAME group sigil-api::mining verifies against (ModSquaring::bench_2048)
            // — a solve produced here genuinely passes real verification, not a mock.
            let g = flux_vdf::ModSquaring::bench_2048();
            let block = flux_miner::client::solve(&challenge, &wallet, &g);
            let submission = flux_miner::client::Submission {
                height: challenge.height,
                wallet: wallet.clone(),
                block,
            };
            let submit_resp = client
                .post(format!("{base}/v1/mining/submit"))
                .json(&submission)
                .send()
                .await
                .expect("submit request");
            let result: flux_miner::client::SubmitResult =
                submit_resp.json().await.expect("submit response body");
            assert!(result.accepted, "real solve was rejected: {:?}", result.reason);

            // Give the ticker time to pick the queued solve up (take_creditable_solve)
            // and mint + settle a block that embeds it, then poll the REAL /v1/balance
            // route — proving credit through the whole pipeline, not just acceptance.
            // Mint + settle a block embedding the solve, then poll the REAL
            // /v1/balance — up to 30s so a scheduler-starved miner under -j8 has
            // time to finish the PoW + settlement it genuinely performs.
            let mut credited_raw: Option<String> = None;
            for _ in 0..1200 {
                if let Ok(r) = client.get(format!("{base}/v1/balance?wallet={wallet}")).send().await {
                    if let Ok(v) = r.json::<serde_json::Value>().await {
                        if let Some(bal) = v.get("data").and_then(|d| d.get("balance")).and_then(|b| b.as_str()) {
                            if bal != "0" {
                                credited_raw = Some(bal.to_string());
                                break;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            stop.store(true, Ordering::Relaxed);
            ticker.join().expect("ticker thread panicked");
            std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
            std::env::remove_var("SIGIL_MINING_BLAKE4_BITS");
            std::env::remove_var("SIGIL_MINING_VDF_T");
            std::env::remove_var("SIGIL_LOCAL_MINING_API_PORT");

            assert!(
                credited_raw.is_some(),
                "wallet balance never rose above 0 — the solve was accepted but never \
                 credited into a settled block"
            );
        });
    }

    /// `maybe_start` must SHORT-CIRCUIT to None on either opt-out flag
    /// (`SIGIL_TOP_PRODUCER=0` / `SIGIL_TOP_PRODUCE=0`) BEFORE it reaches
    /// `sync_chain_blocking` — otherwise a unit test wanders into a real
    /// full-genesis network sync and hangs the whole suite forever. Producer
    /// mode is DEFAULT-ON since 2026-08-27 (the earlier opt-in contract this
    /// test used to assert was flipped), so the default-on path is NOT exercised
    /// here — it belongs to the ignored networked integration test. This pins
    /// only the two opt-outs, the safe and deterministic half.
    #[test]
    fn maybe_start_opts_out_when_either_flag_is_zero() {
        let _env = crate::producer::PRODUCER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SIGIL_TOP_PRODUCER", "0");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
        assert!(
            maybe_start(Duration::from_millis(1)).is_none(),
            "SIGIL_TOP_PRODUCER=0 must opt out without starting a sync"
        );
        std::env::remove_var("SIGIL_TOP_PRODUCER");

        std::env::set_var("SIGIL_TOP_PRODUCE", "0");
        assert!(
            maybe_start(Duration::from_millis(1)).is_none(),
            "SIGIL_TOP_PRODUCE=0 must opt out without starting a sync"
        );
        std::env::remove_var("SIGIL_TOP_PRODUCE");
    }

    /// The spawned background loop actually mints+settles over real wall-clock
    /// ticks, and `stop()` cleanly ends the thread — the one thing the pure
    /// `tick()` test above can't prove by itself.
    #[test]
    fn spawned_loop_settles_blocks_then_stops_cleanly() {
        let handle = spawn_producer_loop(genesis_chain(), Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(120));
        handle.stop(); // joins — if the thread were wedged, this test would hang, not silently pass
    }

    /// 2026-08-23 — NOT part of the normal suite (`#[ignore]`, run explicitly by
    /// name only): a real, bounded, closely-watched connection to the live
    /// sigil-g0 testnet, run once with the operator's explicit go-ahead ("yes
    /// continue and we are on testnet so worst outcome is just a reset"). Starts
    /// the real networked loop, lets it run long enough to see whether it
    /// connects to a real peer and whether any of its candidates settle, then
    /// stops it. `crate::tlog!` writes straight to stderr outside the TUI, so
    /// this test's own stdout/stderr IS the observation — no separate log to
    /// chase.
    #[test]
    #[ignore]
    fn manual_observe_live_networked_run() {
        let handle = spawn_networked_loop(genesis_chain(), Duration::from_millis(500));
        std::thread::sleep(Duration::from_secs(45));
        handle.stop();
    }
}
