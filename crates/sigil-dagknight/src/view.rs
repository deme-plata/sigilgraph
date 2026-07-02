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
}

impl From<&SigilBlockHeaderV0> for BlockView {
    fn from(h: &SigilBlockHeaderV0) -> Self {
        Self {
            hash: h.hash(),
            parent: h.parent_hash,
            merge_parents: h.merge_parents.clone(),
            height: h.height,
            producer: h.producer,
        }
    }
}
