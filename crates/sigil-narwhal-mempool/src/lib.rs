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
//! - [`batch_set`] (BraidPool §12/§17, Phase F) — the `BatchSetRoot`
//!   aggregation: one Merkle root over many `BatchRefV1`s, so a block body
//!   can reference thousands of batches without its header growing.
//! - [`body_mode`] (BraidPool §3.2, Phase F) — the activation gate deciding
//!   whether a block may actually USE a batch-set-root body instead of
//!   inline transactions. Standalone; NOT wired into `sigil-node`'s real
//!   `BlockHeader` this pass — see that module's doc comment for why `n<4`
//!   makes it unsafe today and what SIGIL's real, current answer is.
//! - [`order_meta`] (BraidPool §15, Phase G) — authenticated visibility
//!   metadata a fairness layer could use later, without changing the
//!   availability protocol. Records data only; makes no ordering decision.
//! - [`fair_order_experiment`] (BraidPool §15, Phase G) — the "evaluate
//!   Tilikum-style approaches separately" half: a narrow, honestly-scoped
//!   measurement of validator-index tie-break bias vs a content-hash
//!   tie-break. Explicitly NOT a Tilikum/MRV/Themis/Aequitas implementation
//!   — see that module's doc comment.
//! - [`availability_testnet`] (design doc's Phase D plan) — a deterministic,
//!   in-process SIMULATION of an `n>=4` validator committee: replicated
//!   dissemination, real quorum certificate assembly/rejection, and
//!   recovery after simulated producer loss. NOT a live multi-node network
//!   (SIGIL remains a real `n=1` single-producer chain today) — see that
//!   module's doc comment for exactly what it does and does not prove, and
//!   for a real finding about `BatchCertificate::try_certify`'s calling
//!   contract this work surfaced.
//! - [`shard_ack`] (BraidPool §3.5/§11, follow-up tracked by Phase E) — binds
//!   `shard_index`/`shard_hash` INSIDE a validator's signed ack (the
//!   Reed-Solomon-path counterpart to `types::BatchAck`, which only signs a
//!   whole-batch digest and can't distinguish "holds shard i" from "holds
//!   something"), plus the deterministic per-batch validator->shard
//!   assignment a verifier recomputes rather than trusts. Closes the exact
//!   gap `dissemination.rs`'s Phase E doc comment names. Standalone; not
//!   wired into `sigil-node`/`sigil-api`.

pub mod availability_testnet;
pub mod backend;
pub mod batch_set;
pub mod batch_store;
pub mod body_mode;
pub mod canonical;
pub mod dissemination;
pub mod fair_order_experiment;
pub mod merkle;
pub mod order_meta;
pub mod repair;
pub mod sealer;
pub mod shard_ack;
pub mod types;
pub mod worker;

pub use availability_testnet::{SimCommittee, SimValidator};
pub use backend::MempoolBackend;
pub use batch_set::{batch_set_root, BatchRefV1, BatchSetV1};
pub use batch_store::{BatchStore, BatchStoreMetrics};
pub use body_mode::{activation_mode, sigil_current_body_mode, BodyMode, SIGIL_CURRENT_VALIDATOR_COUNT};
pub use canonical::{BatchHeaderV1, CodingProfile, BATCH_HEADER_VERSION};
pub use dissemination::{reassemble_batch, shard_batch, BatchShard};
pub use fair_order_experiment::{order_content_tiebreak, order_naive_index_tiebreak, synthetic_tie_cohort};
pub use merkle::merkle_root;
pub use order_meta::BatchOrderMetaV1;
pub use repair::{next_repair_peer, repair_priority};
pub use sealer::{BatchSealer, SealPolicy};
pub use shard_ack::{expected_shard_index, AvailabilityCertificateV1, BatchAckV1, BatchStatementV1};
pub use types::{quorum_threshold, BatchAck, BatchCertificate, BlockBatchRef, WorkerBatch, WorkerId};
pub use worker::{BoundedIngestResult, MempoolWorker, ShardedMempool, WorkerLimits};
