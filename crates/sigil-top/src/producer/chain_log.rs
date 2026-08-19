//! Producer-mode durable chain log — INERT STUB, Phase 1 (see `producer/mod.rs`).
//!
//! Phase 2 ports this near-verbatim from `sigil-node/src/chain_log.rs`: a flat
//! append-only file, `[u32 LE length][serde_json bytes]` per block, with a sparse
//! on-disk height→offset index (`chain.idx`) alongside it for O(1) lookups without
//! scanning the whole log. Confirmed this session (read the live source, not assumed)
//! that it depends on nothing but `std::fs`/`serde_json` — no flux-db, no rocksdb, no
//! platform-specific code — so this is a low-risk, high-confidence port, not a rewrite.
