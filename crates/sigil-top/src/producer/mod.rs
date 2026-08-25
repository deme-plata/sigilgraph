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
//! A THIRD, structural gate on top of those two: [`run`]'s loop mints and settles
//! against its own LOCAL braid only — it never broadcasts to the network or accepts
//! peer gossip. So even an instance with both env vars set is invisible to the real
//! sigil-g0 mesh; it's a fully offline proof the minting pipeline works, not a
//! second producer joining the live network. That's the deliberately-deferred next
//! step — see [`run`]'s module doc for why.
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
//!   - [`run`]      — DONE, Phase 3/5 (2026-08-23, operator-directed: "let's do
//!                    this"). The actual loop: seed → (frontier → mint → insert →
//!                    drain-apply) on repeat. Local-mint-only — see the gate #3 note
//!                    above and `run`'s own module doc for exactly what's deferred.
//!
//! Verified this session: `fluxc check -p sigil-top --features producer` and
//! `fluxc test -p sigil-top --features producer` both clean. The default (no
//! `producer` feature) build — what's actually shipped to every user — recompiles
//! byte-for-byte unaffected; re-checked after every change in this module.

pub mod block;
pub mod chain_log;
pub mod coinbase;
pub mod dag;
pub mod mint;
pub mod run;
pub mod sync;

/// Runtime gate 1: has the operator opted this instance into Braid participation?
/// 2026-08-23 (Phase 3, operator-directed: "let's do this") — now real. See
/// `run::maybe_start` for what actually consumes this.
pub fn producer_mode_enabled() -> bool {
    std::env::var("SIGIL_TOP_PRODUCER").map(|v| v == "1").unwrap_or(false)
}

/// Runtime gate 2: has the operator opted this instance into actually minting
/// blocks? Independent of [`producer_mode_enabled`] so a future instance could
/// observe the Braid without producing — `run::maybe_start` requires BOTH gates
/// before starting anything, so today the two are equivalent in practice.
/// 2026-08-23 (Phase 5, operator-directed: "let's do this") — now real.
pub fn should_produce() -> bool {
    std::env::var("SIGIL_TOP_PRODUCE").map(|v| v == "1").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_read_the_real_env_vars() {
        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
        assert!(!producer_mode_enabled());
        assert!(!should_produce());

        std::env::set_var("SIGIL_TOP_PRODUCER", "1");
        std::env::set_var("SIGIL_TOP_PRODUCE", "1");
        assert!(producer_mode_enabled());
        assert!(should_produce());
        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
    }
}
