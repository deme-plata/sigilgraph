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

pub use store::{BlockStore, PrunedHeader, RetentionMode};
