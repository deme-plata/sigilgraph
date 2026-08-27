//! Producer-mode durable chain log — Phase 2 (see `producer/mod.rs`).
//!
//! 2026-08-20: re-exported directly from `sigil_node::chain_log` (a dependency,
//! gated behind this same `producer` feature) rather than hand-ported as a
//! duplicate — same reasoning as [`super::block`]. A flat append-only file,
//! `[u32 LE length][serde_json bytes]` per block, with a sparse on-disk
//! height→offset index (`chain.idx`) alongside it for O(1) lookups without
//! scanning the whole log. Confirmed (read the live source, not assumed) that it
//! depends on nothing but `std::fs`/`serde_json` plus `sigil_node::Block` — no
//! flux-db, no rocksdb, no platform-specific code.

pub use sigil_node::chain_log::*;
