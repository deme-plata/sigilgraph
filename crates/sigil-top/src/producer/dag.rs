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
//! Known, documented limitation carried forward from sigil-dagknight itself (not
//! something this module fixes): blue-score uses raw block COUNT, not
//! difficulty-weighted work — correct only when producers have roughly uniform
//! mining power. Epsilon and a home PC will not. This is an explicit operator
//! judgment call before ever enabling real production against the live mesh —
//! it is not resolved by this port.

pub use sigil_node::dag::*;
