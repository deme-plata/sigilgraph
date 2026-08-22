//! store/ — SIGIL_BRAIDPOOL_v1_1.md §22's suggested split: `memory` (the
//! sealed-batch store, real and tested) and `wal` (durable write-ahead
//! persistence — NOT built; SIGIL's own testnet notes document flux-db
//! persistence as a separate, still-open gap elsewhere in this codebase, and
//! this crate does not attempt to close it here).

pub mod memory;

pub use memory::{BatchStore, BatchStoreMetrics};
