//! sigil-braidpool — the real crate home for SIGIL_BRAIDPOOL_v1_1.md §22's
//! suggested layout. Formerly all of this lived in `sigil-narwhal-mempool`
//! (still Phase-0/A's original crate name, kept alive as a thin
//! backward-compatible re-export shim so `sigil-node`/`sigil-api`/`sigil-top`
//! need zero source changes). This crate is where new BraidPool work lands
//! going forward.
//!
//! Pieces, matching §22's suggested tree (deviations noted where the actual
//! shape differs from the doc's sketch, and why):
//! - [`config`] (§9.2) — `DaEpochConfig`/`DaMode`, the epoch-committed
//!   replication-vs-Reed-Solomon policy. NEW in this crate — never built
//!   before.
//! - [`committee`] (§3.1) — `bft_active`/`max_byzantine`/`availability_quorum`
//!   (named helpers the doc specifies but that were never actually
//!   implemented as callable functions before now) plus a real `Committee`
//!   registry type. NEW.
//! - [`worker`] (v0 §3.1, §5/§14) — sharded, lock-parallel, epoch-salted,
//!   bounded-capacity ingestion. §22 lists `worker.rs` explicitly; no
//!   separate `ingress.rs` split was made — `worker.rs` already covers that
//!   job and an empty `ingress.rs` stub next to it would be pure noise.
//! - [`scheduler`] (§7) — the deficit-round-robin `BatchSource` pull
//!   coordinator. NEW — §7 had zero implementation anywhere before this;
//!   `worker::ShardedMempool::pull` does real worker-level fair-share
//!   round robin already, but not DRR and not the consecutive-selection cap
//!   §7 point 5 calls for. Standalone; not wired to `ShardedMempool`.
//! - [`batch`] (§8, formerly `sealer.rs`) — `BatchSealer`/`SealPolicy`,
//!   local batch construction.
//! - [`canonical`] (§3.4) — the canonical, versioned, domain-separated batch
//!   header + identity.
//! - [`certificate`] (§3.5/§11, formerly `shard_ack.rs`) — `BatchStatementV1`
//!   / `BatchAckV1` (shard_index/shard_hash bound INSIDE the signature) /
//!   `AvailabilityCertificateV1`, the Reed-Solomon-path counterpart to
//!   `types::BatchAck`/`BatchCertificate` (whole-batch, replicated-mode
//!   acks — left as-is in `types`, still correct for that mode).
//! - [`dissemination`] (§9, directory per §22) — [`dissemination::replicated`]
//!   (NEW — a real `ReplicaStore`, generalized out of what
//!   `availability_testnet.rs`'s simulation previously only did ad hoc
//!   in-process), [`dissemination::reed_solomon`] (formerly the crate-root
//!   `dissemination.rs`) and [`dissemination::repair`].
//! - [`store`] (directory per §22) — [`store::memory`] (formerly
//!   `batch_store.rs`). No `store::wal` — durable persistence is a real,
//!   separate, still-open gap (SIGIL's own testnet notes document flux-db
//!   persistence as unwired elsewhere in this codebase); not faked here.
//! - [`batch_set`] (§12/§17) — `BatchSetRoot` aggregation.
//! - [`body_mode`] (§3.2) — the block-body activation gate.
//! - [`metrics`] (§23) — NEW: the actual Prometheus-style counters, real
//!   atomics + text exposition renderer (previously only named in the doc;
//!   `store::memory::BatchStoreMetrics` was the one exception, plain atomics
//!   not wired to any exporter).
//! - [`errors`] — NEW: `DaError`, for callers that want more than the
//!   `Option`/`bool` returns this crate's primary APIs deliberately use.
//! - [`order_meta`] / [`fair_order_experiment`] (§15, Phase G) — visibility
//!   metadata + the narrow tie-break bias measurement.
//! - [`availability_testnet`] — the deterministic in-process `n>=4`
//!   committee simulation (Phase D). Not a live multi-node network — see its
//!   own doc comment.
//! - [`types`] — the original Phase-0/A primitives (`WorkerId`, `WorkerBatch`,
//!   whole-batch `BatchAck`/`BatchCertificate`, `quorum_threshold`). Kept
//!   under this name rather than force-split into §22's `batch.rs`/
//!   `certificate.rs` — those names are already used above for the NEWER,
//!   more specific types, and splitting `types.rs` itself would be pure
//!   file-shuffling churn across ~10 call sites for zero behavior change.
//! - [`merkle`] — the real, tested Merkle tree over per-tx hashes.
//!
//! Standalone and tested. NOT wired into `sigil-node`'s producer loop — see
//! `../SIGIL_NARWHAL_MEMPOOL_v0.md` and `../SIGIL_BRAIDPOOL_v1_1.md`.

pub mod availability_testnet;
pub mod batch;
pub mod batch_set;
pub mod body_mode;
pub mod canonical;
pub mod certificate;
pub mod committee;
pub mod config;
pub mod dissemination;
pub mod errors;
pub mod fair_order_experiment;
pub mod merkle;
pub mod metrics;
pub mod order_meta;
pub mod scheduler;
pub mod store;
pub mod types;
pub mod worker;

pub use availability_testnet::{SimCommittee, SimValidator};
pub use batch::{BatchSealer, SealPolicy};
pub use batch_set::{batch_set_root, BatchRefV1, BatchSetV1};
pub use body_mode::{activation_mode, sigil_current_body_mode, BodyMode, SIGIL_CURRENT_VALIDATOR_COUNT};
pub use canonical::{BatchHeaderV1, CodingProfile, BATCH_HEADER_VERSION};
pub use certificate::{AvailabilityCertificateV1, BatchAckV1, BatchStatementV1};
pub use committee::{availability_quorum, bft_active, max_byzantine, Committee};
pub use config::{DaEpochConfig, DaMode};
pub use dissemination::{reassemble_batch, shard_batch, BatchShard};
pub use errors::DaError;
pub use fair_order_experiment::{order_content_tiebreak, order_naive_index_tiebreak, synthetic_tie_cohort};
pub use merkle::merkle_root;
pub use metrics::MetricsRegistry;
pub use order_meta::BatchOrderMetaV1;
pub use scheduler::{BatchSource, PullScheduler};
pub use store::{BatchStore, BatchStoreMetrics};
pub use types::{quorum_threshold, BatchAck, BatchCertificate, BlockBatchRef, WorkerBatch, WorkerId};
pub use worker::{BoundedIngestResult, MempoolWorker, ShardedMempool, WorkerLimits};

// certificate.rs's own expected_shard_index (needed by dissemination_bench.rs
// and any future caller doing shard-assignment verification directly).
pub use certificate::expected_shard_index;

// dissemination::repair's re-exports, at crate root too (matches the
// original sigil-narwhal-mempool surface `sigil-top`/benches import from).
pub use dissemination::{next_repair_peer, repair_priority};
