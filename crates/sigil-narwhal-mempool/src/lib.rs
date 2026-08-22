//! sigil-narwhal-mempool — backward-compatible re-export shim.
//!
//! All BraidPool implementation work moved to [`sigil_braidpool`]
//! (SIGIL_BRAIDPOOL_v1_1.md §22's real crate layout). This crate now exists
//! only so `sigil-node`/`sigil-api`/`sigil-top`'s existing
//! `sigil_narwhal_mempool::...` imports keep working unchanged — nobody had
//! to touch a single line in those crates for this split. [`backend`]
//! (`MempoolBackend`, the ONE shared mempool handle `sigil-node`'s producer
//! loop and `sigil-api`'s money API both hold) stays here rather than moving
//! to `sigil-braidpool`, since it's specifically the SIGIL-node-integration
//! facade, not BraidPool protocol machinery.
//!
//! New BraidPool work should land in `sigil-braidpool` directly — this crate
//! is not where new modules belong.

pub mod backend;

// Module-level re-exports so `sigil_narwhal_mempool::worker::X`,
// `sigil_narwhal_mempool::types::X`, etc. keep resolving exactly as before
// the split, for any caller reaching past the crate-root re-exports below.
pub use sigil_braidpool::availability_testnet;
pub use sigil_braidpool::batch;
pub use sigil_braidpool::batch_set;
pub use sigil_braidpool::body_mode;
pub use sigil_braidpool::canonical;
pub use sigil_braidpool::certificate;
pub use sigil_braidpool::dissemination;
pub use sigil_braidpool::fair_order_experiment;
pub use sigil_braidpool::merkle;
pub use sigil_braidpool::order_meta;
pub use sigil_braidpool::store;
pub use sigil_braidpool::types;
pub use sigil_braidpool::worker;

pub use backend::MempoolBackend;
pub use sigil_braidpool::{
    activation_mode, batch_set_root, merkle_root, quorum_threshold, sigil_current_body_mode, BatchAck,
    BatchCertificate, BatchHeaderV1, BatchOrderMetaV1, BatchRefV1, BatchSealer, BatchSetV1, BatchStore,
    BatchStoreMetrics, BlockBatchRef, BodyMode, BoundedIngestResult, CodingProfile, MempoolWorker,
    SealPolicy, ShardedMempool, WorkerBatch, WorkerId, WorkerLimits, BATCH_HEADER_VERSION,
    SIGIL_CURRENT_VALIDATOR_COUNT,
};
