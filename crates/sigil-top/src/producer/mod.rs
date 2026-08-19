//! Producer mode — sigil-top running as a full block producer/server, not just a
//! light client. `#[cfg(feature = "producer")]`-gated; off by default.
//!
//! v7.1.40 (grogu-sync-perf, 2026-08-19), Phase 1 of the operator-approved unified-binary
//! plan: this module tree is currently INERT SCAFFOLDING. Nothing here does anything yet
//! — Phase 2 ports the real logic from `sigil-node`'s `main.rs` (block production, the
//! DagKnight/GHOSTDAG braid, coinbase minting) into these files. sigil-node itself stays
//! untouched throughout; this is purely additive to sigil-top.
//!
//! Even once real logic lands, this module stays behind TWO independent gates so a
//! shipped producer-capable binary never silently starts producing:
//!   1. compile-time — the `producer` Cargo feature (this file only exists when it's on).
//!   2. run-time — `SIGIL_TOP_PRODUCER=1` to sync as a real Braid participant (Phase 3),
//!      and separately `SIGIL_TOP_PRODUCE=1` to actually mint+broadcast blocks (Phase 5).
//! Both env checks are currently no-ops (see [`producer_mode_enabled`] /
//! [`should_produce`]) — wiring them to real behavior is later-phase work.
//!
//! Planned module layout (each file currently an inert stub, see its own doc comment):
//!   - [`block`]    — the producer's full `Block` type (header + transition + events),
//!                    ported from `sigil-node/src/block.rs`.
//!   - [`chain_log`] — the flat `[len][serde_json]` durable append-only store, ported
//!                    near-verbatim from `sigil-node/src/chain_log.rs` (plain std::fs,
//!                    already portable — confirmed no flux-db/rocksdb dependency).
//!   - [`coinbase`] — reward/mint-split logic, ported from `sigil-node/src/coinbase.rs`.
//!   - [`mint`]     — `mint_next_block` and friends, ported from `sigil-node`'s
//!                    `main.rs` orchestration.
//!   - [`dag`]      — the DagKnight/GHOSTDAG braid wiring (`dag_seed_braid`,
//!                    `dag_build_frontier`, `dag_drain_apply`,
//!                    `compute_topology_commitment`), ported from `sigil-node`'s
//!                    `main.rs`. This is the module `cathedral.rs`'s own doc comment
//!                    calls the future `run_dagknight_linearize` drop-in slot.

pub mod block;
pub mod chain_log;
pub mod coinbase;
pub mod dag;
pub mod mint;

/// Runtime gate 1: has the operator opted this instance into Braid participation
/// (Phase 3 — sync/observe the real ordering, not yet minting)? Currently always
/// `false` regardless of the env var — wiring this to real behavior is Phase 3 work.
/// The env var is read now (not deferred) so its name is locked in and documented
/// before anything depends on it.
pub fn producer_mode_enabled() -> bool {
    let _requested = std::env::var("SIGIL_TOP_PRODUCER").map(|v| v == "1").unwrap_or(false);
    false // Phase 3 flips this on; Phase 1 stays a documented no-op.
}

/// Runtime gate 2: has the operator opted this instance into actually minting and
/// broadcasting blocks (Phase 5)? Independent of [`producer_mode_enabled`] — an
/// instance can observe the Braid without ever producing. Currently always `false`.
pub fn should_produce() -> bool {
    let _requested = std::env::var("SIGIL_TOP_PRODUCE").map(|v| v == "1").unwrap_or(false);
    false // Phase 5 flips this on; Phase 1 stays a documented no-op.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_are_inert_regardless_of_env() {
        // Phase 1 invariant: no matter what the operator sets, nothing in this
        // module can turn on producer behavior yet. Locks in the "safe until
        // explicitly wired" contract so a later phase can't accidentally regress
        // it without this test failing first.
        std::env::set_var("SIGIL_TOP_PRODUCER", "1");
        std::env::set_var("SIGIL_TOP_PRODUCE", "1");
        assert!(!producer_mode_enabled());
        assert!(!should_produce());
        std::env::remove_var("SIGIL_TOP_PRODUCER");
        std::env::remove_var("SIGIL_TOP_PRODUCE");
    }
}
