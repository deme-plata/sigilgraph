//! Producer-mode `Block` type — Phase 2 (see `producer/mod.rs`).
//!
//! 2026-08-20: re-exported directly from `sigil_node::block` (a dependency, gated
//! behind this same `producer` feature — see `sigil-top/Cargo.toml`) rather than
//! hand-ported as a duplicate. `Block` is
//! `{ header: SigilBlockHeaderV0, transition: StateTransition, events: Vec<SigilEvent> }`.
//! sigil-top's existing `block_store.rs` deliberately stores only `StoredBlock { header, .. }`
//! (header-only, correct for a light client verifying a spine) — producer mode needs the
//! FULL block (header + transition + events) for mint/replay, which is a genuinely
//! different storage shape. Do not try to force this through `block_store.rs` /
//! `skeleton_store.rs`; this module and [`super::chain_log`] are the producer-mode-only
//! storage path, kept separate on purpose.

pub use sigil_node::Block;
