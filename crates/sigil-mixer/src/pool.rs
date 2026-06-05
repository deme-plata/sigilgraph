//! Shielded pool — the global set of unspent commitments + spent nullifiers.
//!
//! This replaces sigil-state's `wallets: BTreeMap<(WalletId, TokenId), u128>`
//! as the source of truth for transferable balance. The pool is intentionally
//! global (one set per token would let outsiders see how much is in each
//! token, leaking activity). Per-token amounts are bound INSIDE each
//! commitment via the `token` arg to `commit()`; the pool itself just sees
//! 32-byte tags.
//!
//! State transitions (called by sigil-state's `commit_state_transition`
//! chokepoint, never directly):
//!
//!   - **Insert** a new commitment (output of a Send / pool LP / reward)
//!   - **Spend** a commitment by exact nullifier match — moves the commitment
//!     into the spent-nullifier set
//!
//! No re-spending: any nullifier that's already in `spent_nullifiers` causes
//! the entire transaction to abort. That's how double-spend is prevented in
//! the shielded model.
//!
//! Phase 0 state is in-memory `BTreeSet`s. Phase 1 backs them with flux-db
//! CFs `shielded_commitments` and `shielded_nullifiers` for persistence +
//! efficient existence checks at chain height H.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::commitment::Commitment;
use crate::nullifier::Nullifier;

/// The shielded pool. Both sets ordered for deterministic hashing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShieldedPool {
    /// All commitments ever inserted, minus those that have been spent.
    /// Phase 0: kept whole. Phase 1 may switch to a Sparse Merkle Tree so
    /// existence-proofs against a chain-height snapshot are O(log N).
    pub commitments: BTreeSet<Commitment>,
    /// Nullifiers that have been used to spend a commitment. Append-only —
    /// once a nullifier is here, the commitment it points to is dead.
    pub spent_nullifiers: BTreeSet<Nullifier>,
}

impl ShieldedPool {
    pub fn new() -> Self { Self::default() }

    /// Insert a new commitment. Idempotent — re-inserting the same
    /// commitment is a no-op (the pool is a set). This matters for
    /// determinism across nodes replaying the same block.
    ///
    /// Returns `true` if the commitment was new.
    pub fn insert(&mut self, c: Commitment) -> bool {
        self.commitments.insert(c)
    }

    /// Spend a commitment by its nullifier. Verifier (sigil-tx layer)
    /// proves via ZK that the nullifier corresponds to a commitment in
    /// the pool without revealing which one. This function then just
    /// records the nullifier as used. Fails if:
    ///   - the nullifier is already in spent_nullifiers (double-spend)
    pub fn spend(&mut self, n: Nullifier) -> Result<(), PoolError> {
        if !self.spent_nullifiers.insert(n) {
            return Err(PoolError::DoubleSpend { nullifier: n });
        }
        Ok(())
    }

    /// Read-only checks for proof verification.

    pub fn contains_commitment(&self, c: &Commitment) -> bool {
        self.commitments.contains(c)
    }

    pub fn is_spent(&self, n: &Nullifier) -> bool {
        self.spent_nullifiers.contains(n)
    }

    pub fn commitment_count(&self) -> usize { self.commitments.len() }
    pub fn spent_count(&self) -> usize { self.spent_nullifiers.len() }

    /// Outstanding shielded supply = commitments - spent. Useful as a UI
    /// number; doesn't reveal any individual balance.
    pub fn outstanding(&self) -> usize {
        self.commitment_count().saturating_sub(self.spent_count())
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum PoolError {
    #[error("double-spend: nullifier {} already used", hex::encode(nullifier.as_slice()))]
    DoubleSpend { nullifier: Nullifier },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::commit;
    use crate::nullifier::derive_nullifier;

    fn token() -> [u8; 32] { [0u8; 32] }

    #[test]
    fn insert_then_spend_succeeds() {
        let mut pool = ShieldedPool::new();
        let c = commit(100, &token(), &[1u8; 32]);
        let n = derive_nullifier(c, b"alice");
        assert!(pool.insert(c));
        assert!(pool.contains_commitment(&c));
        assert!(!pool.is_spent(&n));

        pool.spend(n).unwrap();
        assert!(pool.is_spent(&n));
        assert_eq!(pool.spent_count(), 1);
    }

    #[test]
    fn double_insert_is_idempotent() {
        let mut pool = ShieldedPool::new();
        let c = commit(100, &token(), &[1u8; 32]);
        assert!(pool.insert(c));
        assert!(!pool.insert(c), "second insert returns false (already present)");
        assert_eq!(pool.commitment_count(), 1);
    }

    #[test]
    fn double_spend_is_rejected() {
        let mut pool = ShieldedPool::new();
        let c = commit(100, &token(), &[1u8; 32]);
        let n = derive_nullifier(c, b"alice");
        pool.insert(c);
        pool.spend(n).unwrap();
        let err = pool.spend(n).unwrap_err();
        assert!(matches!(err, PoolError::DoubleSpend { .. }));
    }

    #[test]
    fn outstanding_is_commitments_minus_spent() {
        let mut pool = ShieldedPool::new();
        for i in 0..5u128 {
            let c = commit(i, &token(), &[1u8; 32]);
            pool.insert(c);
        }
        // Spend 2 of them
        let c0 = commit(0, &token(), &[1u8; 32]);
        let c1 = commit(1, &token(), &[1u8; 32]);
        let n0 = derive_nullifier(c0, b"alice");
        let n1 = derive_nullifier(c1, b"alice");
        pool.spend(n0).unwrap();
        pool.spend(n1).unwrap();

        assert_eq!(pool.commitment_count(), 5);
        assert_eq!(pool.spent_count(), 2);
        assert_eq!(pool.outstanding(), 3);
    }

    #[test]
    fn serde_json_roundtrip_preserves_order() {
        let mut pool = ShieldedPool::new();
        for i in 0..3u128 {
            pool.insert(commit(i, &token(), &[1u8; 32]));
        }
        let j = serde_json::to_string(&pool).unwrap();
        let back: ShieldedPool = serde_json::from_str(&j).unwrap();
        assert_eq!(pool, back);
    }
}
