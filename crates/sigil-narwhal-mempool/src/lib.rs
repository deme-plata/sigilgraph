//! sigil-narwhal-mempool — Phase-0 Narwhal-style DAG mempool for SIGIL.
//!
//! Standalone and tested. NOT yet wired into `sigil-node`'s producer loop —
//! see `../SIGIL_NARWHAL_MEMPOOL_v0.md` for the full design and the phased
//! plan for when/how it gets plugged in behind an opt-in flag.
//!
//! Three pieces, matching the design doc's sections:
//! - [`worker`] (§3.1) — sharded, lock-parallel ingestion. Ships now as a
//!   drop-in for today's single-`Mutex` `sigil_tx::Mempool`.
//! - [`types`] (§3.2) — worker batches, availability acks, BFT quorum
//!   certificates (honest about the n=1 degenerate case SIGIL is in today).
//! - [`dissemination`] (§3.3) — erasure-coded batch shards via the
//!   already-proven `flux-aether` Reed-Solomon coder, instead of full
//!   per-peer replication.

pub mod dissemination;
pub mod types;
pub mod worker;

pub use dissemination::{reassemble_batch, shard_batch, BatchShard};
pub use types::{quorum_threshold, BatchAck, BatchCertificate, BlockBatchRef, WorkerBatch, WorkerId};
pub use worker::{MempoolWorker, ShardedMempool};
