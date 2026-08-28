//! The ordering layer's view of a block — extracted from the committed header.
//!
//! The crate never sees transitions/state; bodies stay in `sigil-node`. Both
//! `parent_hash` and `merge_parents` are committed AND producer-signed in the
//! live header (`sigil-header`), so a `BlockView` carries no malleable data.

use serde::{Deserialize, Serialize};
use sigil_header::{BlockHash, SigilBlockHeaderV0};

/// The ordering layer's view of a block: identity + DAG edges + tie-break key
/// material. Extracted from a committed [`SigilBlockHeaderV0`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockView {
    /// Canonical block hash (`SigilBlockHeaderV0::hash()`).
    pub hash: BlockHash,
    /// `header.parent_hash` — the spine edge.
    pub parent: BlockHash,
    /// `header.merge_parents` — the merge edges. Empty for a linear block.
    pub merge_parents: Vec<BlockHash>,
    /// Block height (spine parent height + 1).
    pub height: u64,
    /// `header.producer` (ValidatorId) — second component of the
    /// deterministic `(height, producer, hash)` tie-break.
    pub producer: [u8; 32],
    /// `header.difficulty` — the block's own claim about the work behind it.
    ///
    /// Carried here so the ordering layer can weight a branch by WORK rather
    /// than by block COUNT (see `ghostdag::WorkPolicy`). It is committed and
    /// producer-signed in the header, so it is not malleable.
    ///
    /// ⚠️ Read the units before using this. It is `solve.bits` — a
    /// leading-zero-bit **exponent**, so the work it represents is `2^bits`,
    /// NOT `bits`. And on this chain today it is **0 on almost every block**:
    /// measured 2026-08-28, only 7 of 4096 recent blocks carried a real solve
    /// (0.17%); the rest are producer free-run mints with `difficulty = 0` and
    /// `vdf_proof.t = 0`. Weighting directly by it would therefore give 99.83%
    /// of blocks zero weight and let a handful of blocks decide fork choice —
    /// worse than the count it replaces. That is why `WorkPolicy` defaults to
    /// `UniformCount` and the exponential policy is opt-in.
    #[serde(default)]
    pub difficulty: u64,
}

impl From<&SigilBlockHeaderV0> for BlockView {
    fn from(h: &SigilBlockHeaderV0) -> Self {
        Self {
            hash: h.hash(),
            parent: h.parent_hash,
            merge_parents: h.merge_parents.clone(),
            height: h.height,
            producer: h.producer,
            difficulty: h.difficulty,
        }
    }
}
