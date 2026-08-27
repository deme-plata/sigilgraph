//! Producer-mode coinbase/reward logic — Phase 2 (see `producer/mod.rs`).
//!
//! 2026-08-20: re-exported directly from `sigil_node::coinbase` (reward computation
//! and the master/commons dev-fee split, per that module's own docs) — same
//! single-source-of-truth reasoning as [`super::block`]. Checked: `coinbase.rs` has
//! no `crate::`-relative dependency on `rate_governor.rs` (they're used together by
//! the producer LOOP in `main.rs`, not by each other) — the rate governor itself is
//! a separate, still-unexposed piece, needed only once `mint`/`dag` land.

pub use sigil_node::coinbase::*;
