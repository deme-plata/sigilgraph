//! dissemination/ — SIGIL_BRAIDPOOL_v1_1.md §22's suggested split: replicated
//! (full-copy) vs Reed-Solomon (erasure-coded) availability, plus repair
//! (fetching missing shards from a batch's own certificate signers).

pub mod reed_solomon;
pub mod repair;
pub mod replicated;

pub use reed_solomon::{reassemble_batch, shard_batch, BatchShard};
pub use repair::{next_repair_peer, repair_priority};
pub use replicated::{disseminate_replicated, ReplicaStore};
