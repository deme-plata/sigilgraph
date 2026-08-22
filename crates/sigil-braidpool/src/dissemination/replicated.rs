//! dissemination/replicated.rs — the "replicated" half of §9's hybrid
//! dissemination plane: full-copy availability, no coding. Previously this
//! logic existed only INSIDE `availability_testnet.rs`'s `SimValidator`
//! (receive_replica/has_replica/serve_replica), which is fine for that
//! module's job (simulating a committee) but wasn't a reusable primitive a
//! real node-side store could use directly. This module is that primitive,
//! extracted and generalized: a thread-safe digest-keyed replica store, with
//! no simulation baggage.
//!
//! `availability_testnet.rs` is left as-is (its `SimValidator` keeps its own
//! `HashMap`) rather than refactored to use this — it's a small, already-
//! tested, self-contained simulation harness, and swapping its internals for
//! this type would be pure churn with no behavior change. This module is for
//! a FUTURE real integration (an actual node holding actual replicas), which
//! doesn't exist yet — same standalone-and-inert status as everything else
//! in this crate.

use std::collections::HashMap;
use std::sync::RwLock;

/// A thread-safe store of full batch replicas, keyed by batch digest
/// (`canonical::BatchHeaderV1::batch_id()` in the real system). `&self`-only
/// (RwLock inside), matching this crate's established shared-handle style
/// (see `store::memory::BatchStore`).
#[derive(Default)]
pub struct ReplicaStore {
    replicas: RwLock<HashMap<[u8; 32], Vec<u8>>>,
}

impl ReplicaStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn receive(&self, digest: [u8; 32], bytes: Vec<u8>) {
        self.replicas.write().unwrap().insert(digest, bytes);
    }

    pub fn has(&self, digest: &[u8; 32]) -> bool {
        self.replicas.read().unwrap().contains_key(digest)
    }

    pub fn serve(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        self.replicas.read().unwrap().get(digest).cloned()
    }

    pub fn remove(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        self.replicas.write().unwrap().remove(digest)
    }

    pub fn len(&self) -> usize {
        self.replicas.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Broadcast one batch's bytes to every store in `recipients` — the
/// "replicated availability first" dissemination step (§4's architecture
/// diagram's "Replicated DA" box), generalized from what
/// `availability_testnet.rs::SimCommittee::disseminate_replicated` does
/// in-process for a real multi-store fan-out (e.g. one `ReplicaStore` per
/// local peer connection, in whatever transport eventually calls this).
pub fn disseminate_replicated(recipients: &[&ReplicaStore], digest: [u8; 32], bytes: &[u8]) {
    for store in recipients {
        store.receive(digest, bytes.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_then_serve_roundtrips() {
        let store = ReplicaStore::new();
        let digest = [1u8; 32];
        assert!(!store.has(&digest));
        store.receive(digest, b"payload".to_vec());
        assert!(store.has(&digest));
        assert_eq!(store.serve(&digest), Some(b"payload".to_vec()));
    }

    #[test]
    fn remove_forgets_the_replica() {
        let store = ReplicaStore::new();
        let digest = [2u8; 32];
        store.receive(digest, b"x".to_vec());
        assert_eq!(store.remove(&digest), Some(b"x".to_vec()));
        assert!(!store.has(&digest));
        assert_eq!(store.remove(&digest), None, "removing twice must not panic or resurrect anything");
    }

    #[test]
    fn disseminate_replicated_reaches_every_listed_recipient_only() {
        let a = ReplicaStore::new();
        let b = ReplicaStore::new();
        let c = ReplicaStore::new();
        let digest = [3u8; 32];
        disseminate_replicated(&[&a, &b], digest, b"batch bytes");
        assert!(a.has(&digest));
        assert!(b.has(&digest));
        assert!(!c.has(&digest), "a store not in the recipient list must not receive anything");
    }

    #[test]
    fn len_and_is_empty_track_real_contents() {
        let store = ReplicaStore::new();
        assert!(store.is_empty());
        store.receive([4u8; 32], vec![]);
        store.receive([5u8; 32], vec![]);
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }
}
