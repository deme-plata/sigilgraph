//! config.rs — the epoch-level dissemination policy (SIGIL_BRAIDPOOL_v1_1.md
//! §9): whether a batch is disseminated by full replication, Reed-Solomon
//! coding, or a size-based hybrid of the two. A deliberate, auditable choice
//! committed at an epoch boundary, per §9.1 — not silently decided per-batch
//! by whichever code path happens to run, and not "coding is always better."
//! §3.3's own rule: "ship replication first ... enable Reed-Solomon only
//! after side-by-side measurements show a benefit for a defined workload."

use serde::{Deserialize, Serialize};

use crate::canonical::CodingProfile;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaMode {
    ReplicateOnly,
    ReedSolomonOnly,
    SizeHybrid,
}

/// §9.2's `DaEpochConfig`. `data_shards`/`parity_shards` are only consulted
/// when the resolved profile for a batch is Reed-Solomon (`ReedSolomonOnly`,
/// or `SizeHybrid` above `min_batch_bytes_for_rs`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaEpochConfig {
    pub mode: DaMode,
    pub min_batch_bytes_for_rs: usize,
    pub data_shards: u16,
    pub parity_shards: u16,
}

impl DaEpochConfig {
    /// §3.3's "ship replication first" default: never code, regardless of
    /// batch size.
    pub fn replicate_only() -> Self {
        Self { mode: DaMode::ReplicateOnly, min_batch_bytes_for_rs: usize::MAX, data_shards: 0, parity_shards: 0 }
    }

    /// The exact `(k, parity)` split SIGIL_BRAIDPOOL_v1_1.md §3.3 derives for
    /// a committee of size `n`: `k = f+1`, `parity = n-k`, `f =
    /// floor((n-1)/3)`. Matches the doc's own worked examples exactly:
    /// n=4->(2,2), n=7->(3,4), n=10->(4,6) — the same numbers Phase E's real
    /// `dissemination_bench` measured against.
    pub fn reed_solomon_for_committee(n: usize, mode: DaMode, min_batch_bytes_for_rs: usize) -> Self {
        let f = n.saturating_sub(1) / 3;
        let k = (f + 1) as u16;
        let parity = n.saturating_sub(f + 1) as u16;
        Self { mode, min_batch_bytes_for_rs, data_shards: k, parity_shards: parity }
    }

    /// §9.2's actual decision function: does THIS sealed batch (given its
    /// byte size) use Reed-Solomon coding under this epoch's policy?
    pub fn use_reed_solomon_for(&self, batch_bytes: usize) -> bool {
        match self.mode {
            DaMode::ReplicateOnly => false,
            DaMode::ReedSolomonOnly => true,
            DaMode::SizeHybrid => batch_bytes >= self.min_batch_bytes_for_rs,
        }
    }

    /// The `CodingProfile` a `canonical::BatchHeaderV1` should carry for a
    /// batch of `batch_bytes`, resolving this epoch's policy into the header
    /// schema's own enum.
    pub fn coding_profile_for(&self, batch_bytes: usize) -> CodingProfile {
        if self.use_reed_solomon_for(batch_bytes) {
            CodingProfile::ReedSolomon { data_shards: self.data_shards, parity_shards: self.parity_shards }
        } else {
            CodingProfile::Replicated
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reed_solomon_for_committee_matches_doc_examples() {
        // SIGIL_BRAIDPOOL_v1_1.md §3.3's exact table.
        let c4 = DaEpochConfig::reed_solomon_for_committee(4, DaMode::ReedSolomonOnly, 0);
        assert_eq!((c4.data_shards, c4.parity_shards), (2, 2));
        let c7 = DaEpochConfig::reed_solomon_for_committee(7, DaMode::ReedSolomonOnly, 0);
        assert_eq!((c7.data_shards, c7.parity_shards), (3, 4));
        let c10 = DaEpochConfig::reed_solomon_for_committee(10, DaMode::ReedSolomonOnly, 0);
        assert_eq!((c10.data_shards, c10.parity_shards), (4, 6));
    }

    #[test]
    fn replicate_only_never_uses_reed_solomon() {
        let cfg = DaEpochConfig::replicate_only();
        assert!(!cfg.use_reed_solomon_for(0));
        assert!(!cfg.use_reed_solomon_for(usize::MAX));
        assert_eq!(cfg.coding_profile_for(1_000_000), CodingProfile::Replicated);
    }

    #[test]
    fn reed_solomon_only_always_uses_reed_solomon() {
        let cfg = DaEpochConfig::reed_solomon_for_committee(7, DaMode::ReedSolomonOnly, 0);
        assert!(cfg.use_reed_solomon_for(1));
        assert!(matches!(cfg.coding_profile_for(1), CodingProfile::ReedSolomon { .. }));
    }

    #[test]
    fn size_hybrid_switches_at_the_threshold() {
        let cfg = DaEpochConfig::reed_solomon_for_committee(4, DaMode::SizeHybrid, 1_000);
        assert!(!cfg.use_reed_solomon_for(999), "below threshold must stay replicated");
        assert!(cfg.use_reed_solomon_for(1_000), "at threshold must switch to coded");
        assert!(cfg.use_reed_solomon_for(1_001), "above threshold must stay coded");
    }
}
