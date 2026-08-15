//! sigil-narwhal-mempool — Phase-0/A Narwhal-style DAG mempool for SIGIL.
//!
//! Standalone and tested. NOT yet wired into `sigil-node`'s producer loop —
//! see `../SIGIL_NARWHAL_MEMPOOL_v0.md` and `../SIGIL_BRAIDPOOL_v1_1.md` for
//! the full design and the phased plan for when/how it gets plugged in
//! behind an opt-in flag.
//!
//! Pieces, matching the design docs' sections:
//! - [`worker`] (v0 §3.1, BraidPool §5/§14 Phase A) — sharded, lock-parallel,
//!   epoch-salted, bounded-capacity ingestion. Ships now as a drop-in for
//!   today's single-`Mutex` `sigil_tx::Mempool`.
//! - [`types`] (v0 §3.2) — worker batches, availability acks, BFT quorum
//!   certificates (honest about the n=1 degenerate case SIGIL is in today;
//!   `quorum_threshold` fixed 2026-08-15, see the doc comment on it).
//! - [`canonical`] (BraidPool §3.4, Phase A) — the canonical, versioned,
//!   domain-separated batch header + identity, replacing the earlier bare
//!   digest.
//! - [`merkle`] — a real, tested Merkle tree over per-tx hashes, used for
//!   `canonical::BatchHeaderV1::tx_root`.
//! - [`dissemination`] (v0 §3.3) — erasure-coded batch shards via the
//!   already-proven `flux-aether` Reed-Solomon coder, instead of full
//!   per-peer replication. NOT claimed as novel — see
//!   `SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`.

pub mod backend;
pub mod canonical;
pub mod dissemination;
pub mod merkle;
pub mod types;
pub mod worker;

pub use backend::MempoolBackend;
pub use canonical::{BatchHeaderV1, CodingProfile, BATCH_HEADER_VERSION};
pub use dissemination::{reassemble_batch, shard_batch, BatchShard};
pub use merkle::merkle_root;
pub use types::{quorum_threshold, BatchAck, BatchCertificate, BlockBatchRef, WorkerBatch, WorkerId};
pub use worker::{BoundedIngestResult, MempoolWorker, ShardedMempool, WorkerLimits};
