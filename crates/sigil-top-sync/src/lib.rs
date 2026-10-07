//! # sigil-top-sync — sigil-top's sync engine
//!
//! `block_sync`, `chain_verify`, `gap_sync` and `store_lifecycle`, moved out of the
//! `sigil-top` binary crate on 2026-10-07 with `git mv`. The files are unchanged
//! except `pub(crate)` → `pub` where the binary reaches in. The re-exports below
//! give them the same `crate::…` paths they had inside the binary.

#[macro_use]
extern crate sigil_top_base;

pub use sigil_block_store as block_store;
pub use sigil_top_base::{http_get, instant_ago, tlog, IN_TUI};

pub mod block_sync;
pub mod chain_verify;
pub mod gap_sync;
pub mod store_lifecycle;

pub use store_lifecycle::{build_network_id, sigil_top_db_path};
