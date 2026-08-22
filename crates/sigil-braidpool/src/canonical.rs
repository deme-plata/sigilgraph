//! canonical.rs — Phase A correctness scaffold (SIGIL_BRAIDPOOL_v1_1.md §3.4):
//! a real batch header with domain-separated identity, replacing the earlier
//! bare `BLAKE3(serde_json(WorkerBatch))` digest with a structured, versioned
//! commitment. `serde_json` over a plain struct (no maps) already serializes
//! fields in declaration order deterministically, which is what makes this
//! encoding canonical without needing a separate canonicalization pass.

use serde::{Deserialize, Serialize};

use crate::merkle::merkle_root;
use crate::types::WorkerId;

pub const BATCH_HEADER_VERSION: u16 = 1;

/// How a batch's bytes are (or will be) disseminated. `Replicated` is the
/// only mode Phase A actually uses; `ReedSolomon` exists in the schema now so
/// a later phase doesn't need a header version bump to add it — see
/// SIGIL_BRAIDPOOL_v1_1.md §9.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CodingProfile {
    Replicated,
    ReedSolomon { data_shards: u16, parity_shards: u16 },
}

/// The canonical, versioned identity of a batch. Every consensus-relevant
/// field that distinguishes "this batch" from "a different batch with the
/// same transactions" lives here — round/epoch/worker/sequence, not just the
/// transaction content, so a replay of the same txs under a different
/// context gets a different id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchHeaderV1 {
    pub version: u16,
    pub chain_id: [u8; 32],
    pub epoch: u64,
    pub worker: WorkerId,
    pub sequence: u64,
    pub previous: Option<[u8; 32]>,
    pub tx_count: u32,
    pub uncompressed_len: u32,
    pub tx_root: [u8; 32],
    pub coding: CodingProfile,
    /// Commitment to the shard layout. For `Replicated`, there is exactly one
    /// "shard" (the whole batch), so this equals `tx_root` — there is no
    /// separate sharding structure to commit to yet.
    pub shard_root: [u8; 32],
}

impl BatchHeaderV1 {
    pub fn new_replicated(
        chain_id: [u8; 32],
        epoch: u64,
        worker: WorkerId,
        sequence: u64,
        previous: Option<[u8; 32]>,
        tx_hashes: &[[u8; 32]],
        uncompressed_len: u32,
    ) -> Self {
        let tx_root = merkle_root(tx_hashes);
        Self {
            version: BATCH_HEADER_VERSION,
            chain_id,
            epoch,
            worker,
            sequence,
            previous,
            tx_count: tx_hashes.len() as u32,
            uncompressed_len,
            tx_root,
            coding: CodingProfile::Replicated,
            shard_root: tx_root,
        }
    }

    /// The domain-separated, canonical batch identity. BLAKE3 with an
    /// explicit domain tag — NOT bare `BLAKE3(bytes)` — so a `BatchHeaderV1`
    /// digest can never collide with a digest computed the same way over an
    /// unrelated struct that happens to serialize to the same bytes.
    pub fn batch_id(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"SIGIL/BATCH/V1");
        h.update(&canonical_encode(self));
        h.finalize().into()
    }
}

/// Deterministic encoding for anything consensus-relevant. `serde_json` over
/// a plain struct (no `HashMap`/`BTreeMap` fields) serializes in the struct's
/// DECLARATION order every time — that's what makes this canonical, not an
/// accident of the format. If a future field is ever a map, it needs
/// explicit key sorting before this stays true.
pub fn canonical_encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("BatchHeaderV1 and friends always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u64, tx_hashes: &[[u8; 32]]) -> BatchHeaderV1 {
        BatchHeaderV1::new_replicated([1u8; 32], 7, WorkerId(0), seq, None, tx_hashes, 100)
    }

    #[test]
    fn same_inputs_same_id() {
        let hashes = [[1u8; 32], [2u8; 32]];
        assert_eq!(header(0, &hashes).batch_id(), header(0, &hashes).batch_id());
    }

    #[test]
    fn different_sequence_different_id() {
        let hashes = [[1u8; 32], [2u8; 32]];
        assert_ne!(header(0, &hashes).batch_id(), header(1, &hashes).batch_id());
    }

    #[test]
    fn different_epoch_different_id() {
        let hashes = [[1u8; 32]];
        let mut a = header(0, &hashes);
        let mut b = a.clone();
        b.epoch = a.epoch + 1;
        assert_ne!(a.batch_id(), b.batch_id());
        a.epoch += 0; // no-op, keeps `a` for the assert above meaningful
    }

    #[test]
    fn different_worker_different_id() {
        let hashes = [[1u8; 32]];
        let a = BatchHeaderV1::new_replicated([1u8; 32], 7, WorkerId(0), 0, None, &hashes, 10);
        let b = BatchHeaderV1::new_replicated([1u8; 32], 7, WorkerId(1), 0, None, &hashes, 10);
        assert_ne!(a.batch_id(), b.batch_id());
    }

    #[test]
    fn different_coding_profile_different_id() {
        let hashes = [[1u8; 32]];
        let mut a = header(0, &hashes);
        let mut b = a.clone();
        b.coding = CodingProfile::ReedSolomon { data_shards: 8, parity_shards: 4 };
        assert_ne!(a.batch_id(), b.batch_id());
        a.tx_count += 0; // keep `a` used
    }

    #[test]
    fn different_tx_order_different_id() {
        // tx_root is a Merkle root, so changing tx ORDER (not just content)
        // must change the id — a reordering attack must be detectable.
        let a = header(0, &[[1u8; 32], [2u8; 32]]);
        let b = header(0, &[[2u8; 32], [1u8; 32]]);
        assert_ne!(a.batch_id(), b.batch_id());
    }

    #[test]
    fn shard_root_equals_tx_root_for_replicated() {
        let hashes = [[9u8; 32], [8u8; 32]];
        let h = header(0, &hashes);
        assert_eq!(h.coding, CodingProfile::Replicated);
        assert_eq!(h.shard_root, h.tx_root);
    }

    #[test]
    fn golden_fixed_header_id() {
        // Fixed, non-random inputs -> a batch_id that must never silently
        // drift. If this fails after an intentional encoding change, treat
        // it as a BATCH_HEADER_VERSION bump, not a value to quietly update.
        let h = BatchHeaderV1::new_replicated(
            [0x11u8; 32],
            42,
            WorkerId(3),
            9,
            Some([0x22u8; 32]),
            &[[0xAAu8; 32], [0xBBu8; 32]],
            256,
        );
        let hex: String = h.batch_id().iter().map(|b| format!("{b:02x}")).collect();
        const GOLDEN_HEX: &str = "3750ac4dff18313b3b96c3da4ba23d59161ed3ce75ff61d73befe87b957589da";
        assert_eq!(hex, GOLDEN_HEX);
    }
}
