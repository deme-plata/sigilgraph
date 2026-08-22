//! order_meta.rs — Phase G, part 1 (SIGIL_BRAIDPOOL_v1_1.md §15, §17):
//! authenticated visibility metadata a fairness layer could use LATER,
//! WITHOUT changing the availability protocol. This module only records
//! data; it makes no ordering decision on its own.
//!
//! §15 is explicit: *"Do not invent a home-grown fairness rule in Phase 1.
//! Evaluate Tilikum-style or post-consensus visibility approaches
//! separately."* This module is the "record metadata" half of that
//! instruction. The "evaluate separately" half lives in
//! [`crate::fair_order_experiment`], which is deliberately its own module,
//! deliberately not wired into anything, and deliberately does NOT claim to
//! implement Tilikum, MRV, Themis, Aequitas, or any other specific published
//! fair-ordering protocol — see that module's doc comment for exactly what
//! it does and does not demonstrate.
//!
//! `creator` uses [`WorkerId`], not a separate `ValidatorId` type: SIGIL's
//! mempool layer has no concept yet of "which validator owns which worker"
//! (today there is exactly one producer and no formal validator registry —
//! see `body_mode::SIGIL_CURRENT_VALIDATOR_COUNT`). §15's spec names the
//! field `creator: ValidatorId`; substituting `WorkerId` here is the honest
//! choice available today rather than inventing a validator-identity mapping
//! that doesn't exist in the codebase. Wiring a real per-validator worker
//! ownership concept is future work, tracked the same way Phase F's
//! `activation_mode` tracks its own missing `validator_count` source.

use serde::{Deserialize, Serialize};

use crate::canonical::BatchHeaderV1;
use crate::types::WorkerId;

/// Authenticated ordering-relevant metadata for one batch, matching
/// BraidPool §15's exact field list (`ValidatorId` → `WorkerId`, see module
/// doc comment). Every field here is already committed inside
/// [`BatchHeaderV1`]'s own domain-separated identity except
/// `first_seen_round`, which is necessarily LOCAL — it's when THIS node
/// first observed the batch, not something the batch's producer can commit
/// to in advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOrderMetaV1 {
    pub creator: WorkerId,
    pub epoch: u64,
    pub sequence: u64,
    pub first_seen_round: u64,
    pub tx_root: [u8; 32],
}

impl BatchOrderMetaV1 {
    /// Derive the metadata a node would record the moment it first sees a
    /// batch's header — `first_seen_round` is supplied by the caller (the
    /// node's own DAG round counter), everything else is read straight off
    /// the header's own committed fields.
    pub fn from_header(header: &BatchHeaderV1, first_seen_round: u64) -> Self {
        Self {
            creator: header.worker,
            epoch: header.epoch,
            sequence: header.sequence,
            first_seen_round,
            tx_root: header.tx_root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::CodingProfile;

    fn header(worker: u16, epoch: u64, sequence: u64, tx_root: [u8; 32]) -> BatchHeaderV1 {
        BatchHeaderV1 {
            version: crate::canonical::BATCH_HEADER_VERSION,
            chain_id: [9u8; 32],
            epoch,
            worker: WorkerId(worker),
            sequence,
            previous: None,
            tx_count: 1,
            uncompressed_len: 1,
            tx_root,
            coding: CodingProfile::Replicated,
            shard_root: tx_root,
        }
    }

    #[test]
    fn from_header_carries_the_headers_identity_fields() {
        let h = header(3, 7, 42, [5u8; 32]);
        let meta = BatchOrderMetaV1::from_header(&h, 100);
        assert_eq!(meta.creator, WorkerId(3));
        assert_eq!(meta.epoch, 7);
        assert_eq!(meta.sequence, 42);
        assert_eq!(meta.tx_root, [5u8; 32]);
        assert_eq!(meta.first_seen_round, 100, "first_seen_round is the one field NOT sourced from the header — it's supplied by the caller");
    }

    #[test]
    fn different_headers_produce_different_metadata() {
        let a = BatchOrderMetaV1::from_header(&header(1, 0, 0, [1u8; 32]), 0);
        let b = BatchOrderMetaV1::from_header(&header(2, 0, 0, [1u8; 32]), 0);
        assert_ne!(a, b);
    }

    #[test]
    fn same_header_same_round_is_identical_metadata() {
        let h = header(1, 2, 3, [4u8; 32]);
        assert_eq!(BatchOrderMetaV1::from_header(&h, 9), BatchOrderMetaV1::from_header(&h, 9));
    }
}
