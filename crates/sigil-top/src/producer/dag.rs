//! Producer-mode DagKnight/GHOSTDAG braid wiring — Phase 2 (see `producer/mod.rs`).
//!
//! 2026-08-23 (grogu-producer-unification): re-exported directly from
//! `sigil_node::dag` (braid seed/frontier/drain-apply + the QTFT topology
//! commitment, per that module's own docs) — same single-source-of-truth
//! reasoning as `super::block`/`super::coinbase`. Every function moved over
//! took its state as explicit parameters already (chain, braid, dag_bodies,
//! the bridges) rather than closing over `sigil-node/src/main.rs`'s local event
//! loop, so the move needed zero behavioral change — verified by running
//! sigil-node's own test suite after the move (only pre-existing, unrelated
//! failures remain — see the git log on `coinbase.rs` for that one).
//!
//! Carried forward from sigil-dagknight (not fixed by this module, and now
//! only PARTLY open): selection weighted blue-score by raw block COUNT, which
//! is correct only when producers have roughly uniform mining power — Epsilon
//! and a home PC do not. As of 2026-08-28 the mechanism to fix this exists
//! (`sigil_dagknight::ghostdag::WorkPolicy`) and selection compares accumulated
//! WORK; but it ships as `UniformCount`, which equals the old count exactly, so
//! the hazard is unchanged in behaviour until an operator activates a real work
//! policy. Doing so is consensus-affecting and has a prerequisite: today
//! `header.difficulty` is 0 on 99.83% of blocks, so activating it as-is would
//! be worse than counting.

pub use sigil_node::dag::*;
