//! committee.rs — the §3.1 quorum-floor helpers (`bft_active`/`max_byzantine`/
//! `availability_quorum`), which SIGIL_BRAIDPOOL_v1_1.md specifies but which
//! were never actually implemented as callable functions — `body_mode.rs`
//! enforces the same `n < 4` floor with an inline literal, and nothing
//! elsewhere in this crate exposes it as a reusable check. Plus a real,
//! minimal `Committee` type: an epoch-scoped validator (wallet, pubkey)
//! registry, replacing the ad-hoc `HashMap`/closure callers previously had to
//! hand-roll themselves to use `certificate::AvailabilityCertificateV1::try_certify`.
//!
//! Deliberately NOT wired as a hard gate inside `try_certify` itself:
//! `types::quorum_threshold`'s own doc comment establishes that `n<=1`
//! self-certification is intentional, existing, tested behavior (a
//! single-producer testnet still needs to self-certify its own batches).
//! `bft_active` names the STRONGER guarantee — real Byzantine fault
//! tolerance against a possibly-malicious minority — which only exists at
//! `n>=4`; callers that need that stronger guarantee (like `body_mode.rs`
//! already does) check it themselves, the same way `body_mode.rs` does.

use sigil_state::WalletId;

/// The maximum number of Byzantine (malicious or offline) members a
/// committee of size `n` can tolerate under the standard `n = 3f+1` model.
pub fn max_byzantine(n: usize) -> usize {
    n.saturating_sub(1) / 3
}

/// Is real Byzantine fault tolerance active for a committee of size `n`?
/// SIGIL_BRAIDPOOL_v1_1.md §3.1: disabled below `n=4` even though
/// `max_byzantine`/`availability_quorum` remain mathematically defined for
/// smaller `n` — the helper functions still return sensible numbers at
/// `n<4`, they just don't mean "protected against a malicious minority."
pub fn bft_active(n: usize) -> bool {
    n >= 4
}

/// The conservative `n-f` quorum size (see `types::quorum_threshold`, which
/// this matches exactly — kept here too as the named §3.1 helper the design
/// doc specifies, since some callers reason about committees rather than
/// batches).
pub fn availability_quorum(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    n - max_byzantine(n)
}

/// An epoch-scoped validator registry: which wallets are committee members,
/// and what pubkey each one signs with. SIGIL has no on-chain
/// validator-registry concept yet (same documented gap `body_mode.rs` and
/// `order_meta.rs` both name) — this is a plain in-memory value a caller
/// constructs from whatever source of truth exists (config, a future
/// on-chain registry, or a test harness), not something this crate resolves
/// on its own.
#[derive(Debug, Clone, Default)]
pub struct Committee {
    members: Vec<(WalletId, [u8; 32])>,
}

impl Committee {
    pub fn new(members: Vec<(WalletId, [u8; 32])>) -> Self {
        Self { members }
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn contains(&self, wallet: &WalletId) -> bool {
        self.members.iter().any(|(w, _)| w == wallet)
    }

    pub fn pubkey_for(&self, wallet: &WalletId) -> Option<[u8; 32]> {
        self.members.iter().find(|(w, _)| w == wallet).map(|(_, pk)| *pk)
    }

    pub fn bft_active(&self) -> bool {
        bft_active(self.len())
    }

    pub fn availability_quorum(&self) -> usize {
        availability_quorum(self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bft_active_matches_doc_table() {
        for n in 0..4 {
            assert!(!bft_active(n), "n={n} must be below the BFT floor");
        }
        for n in 4..12 {
            assert!(bft_active(n), "n={n} must be at/above the BFT floor");
        }
    }

    #[test]
    fn max_byzantine_matches_3f_plus_1_model() {
        // n=4 -> f=1, n=7 -> f=2, n=10 -> f=3 (SIGIL_BRAIDPOOL_v1_1.md §3.1's own table).
        assert_eq!(max_byzantine(4), 1);
        assert_eq!(max_byzantine(7), 2);
        assert_eq!(max_byzantine(10), 3);
        assert_eq!(max_byzantine(0), 0);
        assert_eq!(max_byzantine(1), 0);
    }

    #[test]
    fn availability_quorum_matches_types_quorum_threshold_for_n_gte_1() {
        // Same `n-f` formula as types::quorum_threshold for every n>=1 — the
        // two independently-named helpers must never silently drift apart.
        // They deliberately differ only at n=0 (types::quorum_threshold's
        // own doc: "never called with a real empty set, but never zero
        // either" -> returns 1; this function's own §3.1 spec returns 0),
        // an intentional edge case, not a bug — asserted separately below.
        for n in 1..=20usize {
            assert_eq!(availability_quorum(n), crate::types::quorum_threshold(n), "mismatch at n={n}");
        }
    }

    #[test]
    fn availability_quorum_zero_committee_is_zero() {
        assert_eq!(availability_quorum(0), 0);
    }

    #[test]
    fn committee_pubkey_lookup_roundtrip() {
        let w1 = [1u8; 32];
        let w2 = [2u8; 32];
        let pk1 = [9u8; 32];
        let c = Committee::new(vec![(w1, pk1)]);
        assert_eq!(c.pubkey_for(&w1), Some(pk1));
        assert_eq!(c.pubkey_for(&w2), None, "non-member must resolve to no pubkey");
        assert!(c.contains(&w1));
        assert!(!c.contains(&w2));
    }

    #[test]
    fn committee_len_drives_bft_active_and_quorum() {
        let members: Vec<(WalletId, [u8; 32])> = (0..4u8).map(|i| ([i; 32], [i + 100; 32])).collect();
        let c = Committee::new(members);
        assert_eq!(c.len(), 4);
        assert!(c.bft_active());
        assert_eq!(c.availability_quorum(), 3);
    }
}
