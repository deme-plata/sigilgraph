//! Producer-mode block minting — INERT STUB, Phase 1 (see `producer/mod.rs`).
//!
//! Phase 2 ports `build_genesis()` and `mint_next_block()` out of `sigil-node/src/
//! main.rs`'s ~1650-line `run_start()` (there is no existing library seam to import —
//! this is a genuine porting job, not a thin adapter). Phase 5 validates the ported
//! output bit-for-bit against sigil-node's own `mint_next_block()` on shared fixtures
//! before this is ever allowed to run against a real network.
