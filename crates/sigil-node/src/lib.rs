//! sigil-node library surface.
//!
//! The node itself is a binary (`src/main.rs`), but the durable block store is
//! exposed here as a library module so it can be shared by:
//!   - the node (replacing the JSON-snapshot persistence path), and
//!   - the `chronos_sim` / `footprint` measurement bins.
//!
//! Keeping it in a lib (rather than a bin-private `mod`) is what lets a separate
//! bin `use sigil_node::store::BlockStore`.

pub mod store;
/// Also declared `mod coinbase;` in `main.rs` — compiles as two independent
/// copies (bin crate + lib crate), same source file. Safe because `coinbase.rs`
/// has zero `crate::`-relative deps on anything main.rs-private (checked: only
/// imports external crates). Exposed here so `chronos_send_throughput` (and any
/// future chronos bin) can drive real blocks through the real money chokepoint
/// without duplicating `build_block_body_for_shares`.
pub mod coinbase;

pub use store::{BlockStore, PrunedHeader, RetentionMode};
