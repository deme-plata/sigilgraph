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
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sigil_dagknight::{BlockView, Braid};
use sigil_header::BlockHash;
use sigil_tx::SignedTx;

use sigil_node::block::Block;
use sigil_node::chain::ChainTip;
use sigil_node::dag::{
    compute_topology_commitment, dag_build_frontier, dag_drain_apply, dag_seed_braid,
    dag_store_body,
};
use sigil_node::mint::mint_next_block;

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
    send_bridge: sigil_api::send::SendBridge,
    bridge_bridge: sigil_api::bridge::BridgeBridge,
    dex_bridge: sigil_api::dex::DexBridge,
    usds_bridge: sigil_api::usds::UsdsBridge,
    usds_polygon_bridge: sigil_api::usds_bridge::UsdsBridgeBridge,
    shielded_bridge: sigil_api::shielded::ShieldedBridge,
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
        Self {
            chain,
            braid,
            dag_bodies: HashMap::new(),
            mint_hash_to_tx_hashes: HashMap::new(),
            send_bridge: sigil_api::send::SendBridge::new(),
            bridge_bridge: sigil_api::bridge::BridgeBridge::new(None, None),
            dex_bridge: sigil_api::dex::DexBridge::new(),
            usds_bridge: sigil_api::usds::UsdsBridge::new(),
            usds_polygon_bridge: sigil_api::usds_bridge::UsdsBridgeBridge::new(None, None),
            shielded_bridge: sigil_api::shielded::ShieldedBridge::new(),
        }
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

        // Real submissions this instance has authenticated (wallet-signed, via
        // whatever surface exposes SendBridge::submit etc. to callers — not wired
        // yet, so these are empty in practice until a Phase 4 submit endpoint
        // exists). Calling snapshot_for_mint here now, even though it's always
        // empty today, proves the wiring is correct and means a future submit
        // endpoint is a pure addition — no change needed here.
        let txs: Vec<SignedTx> = {
            let mut v = self.send_bridge.snapshot_for_mint();
            v.extend(self.bridge_bridge.snapshot_for_mint());
            v.extend(self.dex_bridge.snapshot_for_mint());
            v.extend(self.usds_bridge.snapshot_for_mint());
            v.extend(self.usds_polygon_bridge.snapshot_for_mint());
            v.extend(self.shielded_bridge.snapshot_for_mint());
            v
        };

        let (block, minted_tx_hashes) =
            mint_next_block(&frontier, merge_parents, &txs, None, None, topology_commitment)?;
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
            &self.send_bridge,
            &self.bridge_bridge,
            &self.dex_bridge,
            &self.usds_bridge,
            &self.usds_polygon_bridge,
            &self.shielded_bridge,
            &mut self.mint_hash_to_tx_hashes,
        );

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

    /// `maybe_start` must be a hard no-op unless BOTH env vars are set — this is
    /// the safety contract the whole module exists to respect. Locks it in the
    /// same way `producer::tests::gates_are_inert_regardless_of_env` already does
    /// for the two gate functions themselves.
    #[test]
    fn maybe_start_is_inert_without_both_env_vars() {
        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
        assert!(maybe_start(Duration::from_millis(1)).is_none());

        std::env::set_var("SIGIL_TOP_PRODUCER", "1");
        assert!(maybe_start(Duration::from_millis(1)).is_none(), "one flag alone must not start it");
        std::env::remove_var("SIGIL_TOP_PRODUCER");

        std::env::set_var("SIGIL_TOP_PRODUCE", "1");
        assert!(maybe_start(Duration::from_millis(1)).is_none(), "the other flag alone must not start it either");
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
