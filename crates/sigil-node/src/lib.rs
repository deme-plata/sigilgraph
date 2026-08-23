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

/// 2026-08-20: `coinbase::producer_wallet()` now calls
/// `crate::producer_signing::reconcile_producer_wallet` — dual-declared here
/// for the same reason `coinbase` itself is (so the lib-crate compile of
/// `coinbase.rs` resolves `crate::producer_signing`). Zero `crate::`-relative
/// deps of its own (only `sigil_header`/`sigil_state`/`ed25519_dalek`/`std`),
/// so this is safe by the same test as `coinbase`/`block`/`chain_log` above.
pub mod producer_signing;

/// 2026-08-20 (unified-binary Phase 2, operator-directed): same dual-declaration
/// pattern as `coinbase` above. Both `block.rs` (zero `crate::`-relative deps —
/// fully self-contained) and `chain_log.rs` (depends only on `crate::block::Block`,
/// exposed right here) are safe to share this way. Exposed so `sigil-top`'s
/// `producer` feature can depend on `sigil-node` directly and use the REAL block
/// type + REAL durable store, instead of maintaining a hand-ported duplicate that
/// could silently drift from the version `sigil-node` actually runs.
pub mod chain;
pub mod frontier;
pub mod block;
pub mod chain_log;

pub use store::{BlockStore, PrunedHeader, RetentionMode};
pub use block::Block;
