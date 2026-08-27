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
    // What still protects a fresh install, and is NOT weakened by this flip:
    // `run::maybe_start` refuses to produce unless `sync::sync_chain` reached the live tip,
    // and treats a PARTIAL sync as failure rather than a lesser success (sync.rs,
    // 2026-08-25). So "out of the box" means: sync fully, THEN produce — never mint from a
    // stale tip.
    //
    // Knock-on effect, and the second half of the same operator request ("fix miners server
    // to point to it self localhost"): once this starts, `mining_api` binds
    // 127.0.0.1:18183 and `engine_node_url()` returns the LOCAL url instead of the central
    // node. A rig therefore mines against its own node automatically, with no
    // configuration — that wiring already existed and was simply unreachable while
    // production was opt-in.
    !matches!(std::env::var("SIGIL_TOP_PRODUCE").as_deref(), Ok("0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_read_the_real_env_vars() {
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
