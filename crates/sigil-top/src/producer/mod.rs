//! Producer mode — sigil-top running as a full block producer/server, not just a
//! light client. `#[cfg(feature = "producer")]`-gated; off by default.
//!
//! v7.1.40 (grogu-sync-perf, 2026-08-19), Phase 1: scaffolding only. Phase 2
//! (2026-08-20, operator-directed: "one binary both client and server, like
//! Quillon Graph") is IN PROGRESS. Course-corrected from the original Phase 1 plan:
//! rather than hand-PORTING (duplicating) sigil-node's logic into this tree —
//! which risks silent drift between two copies of consensus-adjacent code —
//! sigil-top's `producer` feature now DEPENDS on `sigil-node` as a library and
//! re-exports the real thing. sigil-node itself stays untouched as a binary; this
//! is purely additive on both sides (a new `pub mod` here, a new optional dep
//! there).
//!
//! Even with everything below now real, this module stays behind TWO independent
//! gates so a shipped producer-capable binary never silently starts producing:
//!   1. compile-time — the `producer` Cargo feature (this file only exists when it's on).
//!   2. run-time — `SIGIL_TOP_PRODUCER=1` AND `SIGIL_TOP_PRODUCE=1` (both required —
//!      see [`producer_mode_enabled`]/[`should_produce`]/[`run::maybe_start`]).
//! CORRECTION (2026-08-25): the paragraph that used to stand here claimed a third,
//! structural gate — that [`run`]'s loop only ever mints/settles against its own
//! LOCAL braid and never broadcasts, making even a fully-enabled instance invisible
//! to the real sigil-g0 mesh. That stopped being true on 2026-08-24, when
//! [`run::maybe_start`] was wired to [`sync`]'s real snapshot+tail-replay bootstrap
//! and [`run`]'s `spawn_networked_loop` (a REAL `flux_p2p::NetworkManager` that
//! subscribes to and publishes on `sigil_net::TOPIC_BLOCKS`) — this doc simply never
//! got updated to say so. As of today, `maybe_start()` with BOTH env vars set really
//! does sync from a real running node and join sigil-g0 as a real second producer;
//! there is currently no separate compile-time-safe "local-only, never touches the
//! network" mode. `run`'s own module doc has the accurate, detailed version of this.
//!
//! Module layout — status per module, not "planned":
//!   - [`block`]    — DONE. Real re-export of `sigil_node::Block` (header +
//!                    transition + events). Zero duplication.
//!   - [`chain_log`] — DONE. Real re-export of `sigil_node::chain_log::ChainLog`
//!                    (flat append-only `[len][serde_json]` store + sparse
//!                    height→offset index). Zero duplication.
//!   - [`coinbase`] — DONE. Real re-export of `sigil_node::coinbase::*` (reward
//!                    computation, master/commons dev-fee split). Zero duplication.
//!   - [`mint`]     — DONE (2026-08-23). Real re-export of `sigil_node::genesis` +
//!                    `sigil_node::mint::mint_next_block` — zero duplication, both
//!                    verified byte-identical/behavior-identical to sigil-node's own
//!                    copies via cross-crate tests.
//!   - [`dag`]      — DONE (2026-08-23). Real re-export of `sigil_node::dag::*`
//!                    (`dag_seed_braid`, `dag_build_frontier`, `dag_drain_apply`,
//!                    `compute_topology_commitment` + helpers). Every function
//!                    turned out to already take its state as explicit parameters
//!                    rather than closing over `main.rs`'s local event-loop state,
//!                    so this was a pure relocation, not a rewrite.
//!   - [`sync`]     — DONE (2026-08-24). Snapshot bootstrap + P2P tail replay
//!                    against a real running node — what makes [`run::maybe_start`]
//!                    mint on top of the REAL chain instead of a fresh local genesis.
//!                    Refuses (returns `None`) rather than falling back to a fresh
//!                    genesis on any failure — see its own module doc.
//!   - [`run`]      — DONE, Phase 3/5 (2026-08-23) + sync-then-produce + real network
//!                    join (2026-08-24, operator-directed: "let's do this" / "every
//!                    user downloading sigil top will be full node operator"). The
//!                    actual loop: sync → (frontier → mint → insert → drain-apply) on
//!                    repeat, publishing candidates to the real sigil-g0 mesh.
//!   - [`mining_api`] — DONE (2026-08-25, operator-directed: "let a miner mine
//!                    against their OWN locally-running node instead of always
//!                    hitting the central Epsilon node"). Starts a real local
//!                    `sigil-api` HTTP server (`sigil_api::router`, unmodified) once
//!                    [`run::maybe_start`] is actually running, sharing the SAME
//!                    `MiningBridge`/money-bridge Arcs [`run::ProducerState::tick`]
//!                    drains and publishes tips into — closing the gap the earlier
//!                    version of this file's module doc called "the deliberately-
//!                    deferred next step": until this, nothing in this module ever
//!                    listened on an HTTP port, so a miner pointed at a local
//!                    producer had nothing to talk to.
//!
//! Verified this session: `fluxc check -p sigil-top --features producer` and
//! `fluxc test -p sigil-top --features producer` both clean. The default (no
//! `producer` feature) build — what's actually shipped to every user — recompiles
//! byte-for-byte unaffected; re-checked after every change in this module.

pub mod block;
pub mod chain_log;
pub mod coinbase;
pub mod dag;
pub mod mining_api;
pub mod mint;
pub mod realization;
pub mod run;
pub mod sync;

/// Runtime gate 1: has the operator opted this instance into Braid participation?
/// 2026-08-23 (Phase 3, operator-directed: "let's do this") — now real. See
/// `run::maybe_start` for what actually consumes this.
pub fn producer_mode_enabled() -> bool {
    // DEFAULT-ON as of 2026-08-27 (operator-directed: "yes . but should have full sync
    // first"). Every sigil-top is a full node that joins the braid; `SIGIL_TOP_PRODUCER=0`
    // opts out. The "full sync first" condition is not added here — it is already
    // structural in `run::maybe_start`, which refuses to start unless `sync::sync_chain`
    // reached the live tip, and treats a PARTIAL sync as failure rather than as a lesser
    // success (see that module's 2026-08-25 finding: producing from a height tens of
    // thousands of blocks behind the tip is a silent fork by construction).
    !matches!(std::env::var("SIGIL_TOP_PRODUCER").as_deref(), Ok("0"))
}

/// Runtime gate 2: has the operator opted this instance into actually minting
/// blocks? Independent of [`producer_mode_enabled`] so a future instance could
/// observe the Braid without producing — `run::maybe_start` requires BOTH gates
/// before starting anything, so today the two are equivalent in practice.
/// 2026-08-23 (Phase 5, operator-directed: "let's do this") — now real.
pub fn should_produce() -> bool {
    // DEFAULT-ON as of 2026-08-27 (operator-directed: "fix it so the new unified binary
    // just works out of the box producing blocks"). `SIGIL_TOP_PRODUCE=0` opts out.
    //
    // This flips the opt-IN chosen a few hours earlier the same day. The operator was told
    // the hazard twice, in writing, and chose default-on both times; it is recorded here so
    // the choice is legible to whoever reads this next rather than living only in a chat
    // log:
    //
    //   GHOSTDAG blue-score counts raw block COUNT, not difficulty-weighted work (see
    //   `dag.rs`). Two producers with wildly different hashpower — a 525 MH/s rig and a
    //   laptop — contribute blocks that weigh the SAME, so the heaviest-branch rule stops
    //   tracking actual work. That is a fork hazard, and default-on makes it live rather
    //   than theoretical. Work-weighting blue-score is the fix; this gate never was.
    //
    // STATUS 2026-08-28 — the fix is HALF done, and the half that is missing is not the
    // part you would guess. The mechanism now exists and is wired end to end:
    // `header.difficulty` reaches the ordering layer (`BlockView` -> `BitfieldDag` ->
    // `GhostdagStore`), blue WORK accumulates alongside blue score, and `select_tip`
    // compares work. It ships as `WorkPolicy::UniformCount`, which is numerically
    // identical to the old count — asserted by test — so nothing about selection has
    // changed yet.
    //
    // What blocks activation is the DATA, not the code. Measured against the live chain
    // that day: `header.difficulty` is `solve.bits`, an EXPONENT (work is 2^bits, not
    // bits), and it is 0 on 4089 of 4096 recent blocks — those are producer free-run
    // mints carrying `difficulty = 0` AND `vdf_proof.t = 0`, i.e. no proof of work at
    // all. Switching to a real work metric today would hand 99.83% of blocks zero weight
    // and let 7 blocks decide fork choice. That is strictly worse than the count it
    // replaces.
    //
    // So the honest statement of the hazard is now: the ordering layer can weight by work
    // the moment blocks tell the truth about their work. Until every block carries a
    // meaningful work claim, `Exponential` is a loaded gun and the hazard stands as
    // written above.
    //
    // What still protects a fresh install, and is NOT weakened by this flip:
    // `run::maybe_start` refuses to produce unless `sync::sync_chain` reached the live tip,
    // and treats a PARTIAL sync as failure rather than a lesser success (sync.rs,
    // 2026-08-25). So "out of the box" means: sync fully, THEN produce — never mint from a
    // stale tip.
    //
    // Knock-on effect, and the second half of the same operator request ("fix miners server
    // to point to it self localhost"): once this starts, `mining_api` binds
    // 127.0.0.1:18185 and `engine_node_url()` returns the LOCAL url instead of the central
    // node. A rig therefore mines against its own node automatically, with no
    // configuration — that wiring already existed and was simply unreachable while
    // production was opt-in.
    !matches!(std::env::var("SIGIL_TOP_PRODUCE").as_deref(), Ok("0"))
}

// ── Runtime gate 3 + status (2026-09-17, unified binary) ──────────────────────────────
//
// Viktor: "i want a unified binary with sigil top node and sigil node so when users download
// sigil graph they are actually also real nodes if they have full archive. that way it makes
// sense to press F for full archive in tui — because afterwards it produces real blocks and
// rewards etc". So the sync-then-produce bootstrap no longer starts on every launch: it waits
// until the operator has asked for a FULL node — the persisted sync mode is "full" (the F key,
// `--sync`), or `SIGIL_TOP_PRODUCE=1` is set explicitly. A light-monitor user (the default)
// gets no multi-hour replay running behind the dashboard unasked. Gates 1 and 2 above still
// bind: `SIGIL_TOP_PRODUCER=0` / `SIGIL_TOP_PRODUCE=0` mean "never", whatever F says.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

static FULL_NODE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The g2 producer's hybrid ids, in `sigil-node`'s `SIGIL_TRUSTED_PRODUCER_ID_HEX` format
/// (`hex[@from],…`): the key retired on 2026-09-15 (signed every checkpoint before block
/// 18,874,368) and the rotated one (from there on). Same list happysrv and node3 carry.
pub const DEFAULT_TRUSTED_PRODUCER_IDS: &str =
    "ccb24eab7f4d975d9aac142837bcea8a4f8a559b6497c7fbe83b91118d78bb5b,0512706efc948876251f9c5010106dea11cf3cdc49d9193ce98a4d6e9f222fb4";

/// The operator asked for a full node (F key / persisted "full" / `SIGIL_TOP_PRODUCE=1`).
pub fn request_full_node() {
    FULL_NODE_REQUESTED.store(true, Ordering::Relaxed);
}
/// Back to light monitor BEFORE the bootstrap started: nothing to stop, just don't start.
/// (Once production is running it keeps its synced state — see the F handler's toast.)
pub fn withdraw_full_node_request() {
    FULL_NODE_REQUESTED.store(false, Ordering::Relaxed);
}
pub fn full_node_requested() -> bool {
    FULL_NODE_REQUESTED.load(Ordering::Relaxed)
        || matches!(std::env::var("SIGIL_TOP_PRODUCE").as_deref(), Ok("1"))
}

/// What the producer is doing right now, for the dashboard. Written from the bootstrap
/// thread (`sync.rs`) and the loop (`run.rs`), read once per frame.
pub mod status {
    use super::*;

    pub const IDLE: u8 = 0;      // waiting for F (or produce is off)
    pub const SYNCING: u8 = 1;   // replaying the chain, height in `replayed`
    pub const PRODUCING: u8 = 2; // synced; the networked loop is minting + gossiping
    pub const REFUSED: u8 = 3;   // bootstrap failed — reason in `message()`

    static PHASE: AtomicU8 = AtomicU8::new(IDLE);
    static REPLAYED: AtomicU64 = AtomicU64::new(0);
    static REPLAY_TARGET: AtomicU64 = AtomicU64::new(0);
    static MINTED: AtomicU64 = AtomicU64::new(0);
    static LAST_MINTED_HEIGHT: AtomicU64 = AtomicU64::new(0);
    static SETTLED: AtomicU64 = AtomicU64::new(0);
    static PEERS: AtomicU64 = AtomicU64::new(0);
    static INGESTED: AtomicU64 = AtomicU64::new(0);
    /// Highest block height seen on live gossip — the network's tip as this loop knows
    /// it. `settled` trailing this by more than the finality depth for long is the
    /// "frozen behind the network" fault the resync lane in `run.rs` repairs.
    static GOSSIP_TIP: AtomicU64 = AtomicU64::new(0);
    /// Parked (parent-missing) views in the braid, sampled after each tick.
    static PARKED: AtomicU64 = AtomicU64::new(0);
    /// Mid-run tail resyncs performed (each one is a logged `[producer] RESYNC` line).
    static RESYNCS: AtomicU64 = AtomicU64::new(0);
    static MESSAGE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    /// The newest settled `(height, block hash)` pairs, for the DATA INTEGRITY card's
    /// exact-at-height comparison against the network (8,192 ≈ 17 min at 8 blk/s).
    static SETTLED_RING: std::sync::Mutex<std::collections::VecDeque<(u64, [u8; 32])>> =
        std::sync::Mutex::new(std::collections::VecDeque::new());
    const SETTLED_CAP: usize = 8192;

    pub fn push_settled(height: u64, hash: [u8; 32]) {
        if let Ok(mut g) = SETTLED_RING.lock() {
            if g.back().map(|(h, _)| *h >= height).unwrap_or(false) { return; }
            g.push_back((height, hash));
            while g.len() > SETTLED_CAP { g.pop_front(); }
        }
    }
    /// Our settled block hash at exactly `height`, if we still hold it.
    pub fn settled_hash_at(height: u64) -> Option<[u8; 32]> {
        let g = SETTLED_RING.lock().ok()?;
        let (first, _) = *g.front()?;
        if height < first { return None; }
        let idx = (height - first) as usize;
        g.get(idx).filter(|(h, _)| *h == height).map(|(_, x)| *x)
    }
    /// Highest settled height in the ring (0 = nothing yet).
    pub fn settled_top() -> u64 { SETTLED_RING.lock().ok().and_then(|g| g.back().map(|(h, _)| *h)).unwrap_or(0) }

    pub fn set_phase(p: u8) { PHASE.store(p, Ordering::Relaxed); }
    pub fn phase() -> u8 { PHASE.load(Ordering::Relaxed) }
    pub fn set_replayed(h: u64) { REPLAYED.store(h, Ordering::Relaxed); }
    pub fn replayed() -> u64 { REPLAYED.load(Ordering::Relaxed) }
    /// The best tip the light engine / node has told us about — lets the dashboard show a
    /// fraction while the replay runs. 0 = unknown.
    pub fn set_replay_target(h: u64) { REPLAY_TARGET.store(h, Ordering::Relaxed); }
    pub fn replay_target() -> u64 { REPLAY_TARGET.load(Ordering::Relaxed) }
    pub fn note_minted(height: u64) {
        MINTED.fetch_add(1, Ordering::Relaxed);
        LAST_MINTED_HEIGHT.store(height, Ordering::Relaxed);
    }
    pub fn minted() -> u64 { MINTED.load(Ordering::Relaxed) }
    pub fn last_minted_height() -> u64 { LAST_MINTED_HEIGHT.load(Ordering::Relaxed) }
    pub fn set_settled(h: u64) { SETTLED.store(h, Ordering::Relaxed); }
    pub fn settled() -> u64 { SETTLED.load(Ordering::Relaxed) }
    pub fn set_peers(n: u64) { PEERS.store(n, Ordering::Relaxed); }
    /// Live blocks taken off gossip (the proof the loop is on the network at all).
    pub fn note_ingested(n: u64) { INGESTED.fetch_add(n, Ordering::Relaxed); }
    pub fn ingested() -> u64 { INGESTED.load(Ordering::Relaxed) }
    pub fn peers() -> u64 { PEERS.load(Ordering::Relaxed) }
    pub fn note_gossip_tip(h: u64) { GOSSIP_TIP.fetch_max(h, Ordering::Relaxed); }
    pub fn gossip_tip() -> u64 { GOSSIP_TIP.load(Ordering::Relaxed) }
    /// How far the settled chain trails the newest gossiped block (0 when unknown).
    pub fn lag() -> u64 { gossip_tip().saturating_sub(settled()) }
    pub fn set_parked(n: u64) { PARKED.store(n, Ordering::Relaxed); }
    pub fn parked() -> u64 { PARKED.load(Ordering::Relaxed) }
    pub fn note_resync() { RESYNCS.fetch_add(1, Ordering::Relaxed); }
    pub fn resyncs() -> u64 { RESYNCS.load(Ordering::Relaxed) }
    pub fn set_message(m: impl Into<String>) {
        if let Ok(mut g) = MESSAGE.lock() { *g = m.into(); }
    }
    pub fn message() -> String {
        MESSAGE.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// One line for the dashboard, or `None` when there is nothing to say (light monitor,
    /// production off). Plain text — the renderer adds colour.
    pub fn line() -> Option<String> {
        match phase() {
            IDLE => None,
            SYNCING => {
                let h = replayed();
                let t = replay_target();
                Some(if t > h && t > 0 {
                    format!("full node: replaying chain h={h} of ~{t} ({:.1}%) — produces once at the tip", 100.0 * h as f64 / t as f64)
                } else {
                    format!("full node: replaying chain h={h} — produces once at the tip")
                })
            }
            PRODUCING => {
                // The lag is the one number that says whether this node is actually
                // following the network: ~final_depth (512) is healthy, thousands means
                // the settled chain is frozen and the resync lane is (or should be) at work.
                let lag = lag();
                let behind = if lag > 2048 {
                    format!(" · ⚠ {lag} BEHIND the network ({} parked, {} resyncs)", parked(), resyncs())
                } else {
                    String::new()
                };
                Some(if minted() == 0 {
                    format!(
                        "full node: LIVE — settled h={} · net h={} · gossip in {} · peers {}{behind} · mints when a rig solves or a tx is pending",
                        settled(), gossip_tip(), ingested(), peers()
                    )
                } else {
                    format!(
                        "full node: PRODUCING — minted {} (last h={}) · settled h={} · net h={} · gossip in {} · peers {}{behind}",
                        minted(), last_minted_height(), settled(), gossip_tip(), ingested(), peers()
                    )
                })
            }
            _ => Some(format!("full node: refused — {}", message())),
        }
    }
}

/// Serializes tests (here and in `run`) that mutate the process-global
/// SIGIL_TOP_PRODUCER / SIGIL_TOP_PRODUCE gate vars — cargo's parallel test runner
/// otherwise lets two race on them (one sets while another reads the gate).
#[cfg(test)]
pub(crate) static PRODUCER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Gate 3: nothing starts until the operator asks; the ask can be withdrawn while
    /// still waiting; `SIGIL_TOP_PRODUCE=1` counts as an ask on its own.
    #[test]
    fn full_node_request_gate() {
        let _g = PRODUCER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SIGIL_TOP_PRODUCE");
        withdraw_full_node_request();
        assert!(!full_node_requested(), "fresh install: light monitor, no replay behind the dashboard");
        request_full_node();
        assert!(full_node_requested(), "F pressed → full node requested");
        withdraw_full_node_request();
        assert!(!full_node_requested(), "F again before the bootstrap started → withdrawn");
        std::env::set_var("SIGIL_TOP_PRODUCE", "1");
        assert!(full_node_requested(), "explicit SIGIL_TOP_PRODUCE=1 is an ask by itself");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
    }

    /// The dashboard line says nothing while idle and reports a fraction while syncing.
    #[test]
    fn status_line_shapes() {
        status::set_phase(status::IDLE);
        assert!(status::line().is_none());
        status::set_phase(status::SYNCING);
        status::set_replayed(500);
        status::set_replay_target(1000);
        let l = status::line().unwrap();
        assert!(l.contains("h=500") && l.contains("50.0%"), "{l}");
        status::set_phase(status::REFUSED);
        status::set_message("no peers");
        assert!(status::line().unwrap().contains("no peers"));
        status::set_phase(status::IDLE);
    }

    #[test]
    fn gates_read_the_real_env_vars() {
        let _env = PRODUCER_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Unset = ON. Producing is the default posture now; the env vars are an OPT-OUT.
        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
        assert!(producer_mode_enabled(), "unset must mean ON — braid participation is the default");
        assert!(should_produce(), "unset must mean ON — the binary produces out of the box");

        std::env::set_var("SIGIL_TOP_PRODUCER", "1");
        std::env::set_var("SIGIL_TOP_PRODUCE", "1");
        assert!(producer_mode_enabled());
        assert!(should_produce());

        // Only an explicit "0" opts out — a stray value must not silently disable a node.
        std::env::set_var("SIGIL_TOP_PRODUCER", "0");
        std::env::set_var("SIGIL_TOP_PRODUCE", "0");
        assert!(!producer_mode_enabled(), "explicit 0 opts out of the braid");
        assert!(!should_produce(), "explicit 0 opts out of minting");

        // Asymmetric ON PURPOSE: braid participation must not be disabled by a stray
        // value, and minting must not be ENABLED by one.
        std::env::set_var("SIGIL_TOP_PRODUCER", "yes");
        std::env::set_var("SIGIL_TOP_PRODUCE", "yes");
        assert!(producer_mode_enabled(), "a non-0 value must not disable braid participation");
        assert!(should_produce(), "a non-0 value must not disable production");

        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
    }
}
