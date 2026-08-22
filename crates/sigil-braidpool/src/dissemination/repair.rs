//! repair.rs — Phase E, "deterministic peer repair" (SIGIL_BRAIDPOOL_v1_1.md
//! §10, §17): given a batch's certified signers, decide WHICH peer to ask
//! next for a missing shard, deterministically — so a node reconstructing a
//! batch doesn't broadcast a request to everyone (wasteful) or always ask
//! the same "first" validator in some list (concentrates load on one peer
//! across every repair, unfair and a soft target).
//!
//! KNOWN GAP, stated plainly rather than papered over: SIGIL_BRAIDPOOL_v1_1.md
//! §3.5/§11's corrected design binds `shard_index` into the SIGNED
//! `BatchAckMessageV1`, so a real deployment would know exactly which shard
//! each signer holds before asking. `types::BatchAck` was never updated to
//! carry that field — it's still the Phase-0/A shape (digest + validator +
//! sig, no shard_index). This module's peer-priority function therefore
//! ranks "which signer to ask next," not "which signer holds shard i
//! specifically" — a real repair flow built on top of this would currently
//! have to ask a candidate peer for the general batch and let THEM tell you
//! which shard they're returning, rather than requesting a specific index.
//! Closing this gap (updating `BatchAck`/`BatchCertificate` to the corrected
//! shape) is tracked as follow-up work, not done in this pass — changing an
//! already-shipped, already-tested signed-message shape is a bigger,
//! separately-reviewable change than "add a repair-ordering function."

use sigil_state::WalletId;

use crate::types::BatchCertificate;

/// Deterministic priority order for repairing `digest`, given the validators
/// who signed its [`BatchCertificate`]. Uses `BLAKE3(digest || validator)` as
/// a per-(batch, validator) priority key, then sorts ascending — every node
/// computing this over the SAME certificate gets the SAME order (no
/// coordination needed), but the order varies PER BATCH (so repeated repairs
/// across many batches spread load across the signer set rather than always
/// hammering whichever validator happens to sort first in some fixed list).
pub fn repair_priority(cert: &BatchCertificate, digest: [u8; 32]) -> Vec<WalletId> {
    let mut candidates: Vec<(WalletId, [u8; 32])> = cert
        .acks
        .iter()
        .map(|ack| {
            let mut h = blake3::Hasher::new();
            h.update(b"SIGIL/REPAIR/V1");
            h.update(&digest);
            h.update(&ack.validator);
            (ack.validator, h.finalize().into())
        })
        .collect();
    candidates.sort_by(|a, b| a.1.cmp(&b.1));
    candidates.into_iter().map(|(w, _)| w).collect()
}

/// The next peer to ask, given who's already been tried (and failed / not
/// yet responded) this repair attempt. `None` if every signer has already
/// been tried — the caller should treat that as "repair failed, all known
/// holders exhausted," not retry the same peer.
pub fn next_repair_peer(cert: &BatchCertificate, digest: [u8; 32], already_tried: &[WalletId]) -> Option<WalletId> {
    repair_priority(cert, digest)
        .into_iter()
        .find(|w| !already_tried.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BatchAck;
    use sigil_tx::ed25519_keygen;

    fn cert_with_n_signers(digest: [u8; 32], n: usize) -> (BatchCertificate, Vec<WalletId>) {
        let mut acks = Vec::new();
        let mut wallets = Vec::new();
        for _ in 0..n {
            let (sk, pk, wallet) = ed25519_keygen();
            acks.push(BatchAck::sign(digest, &sk, &pk));
            wallets.push(wallet);
        }
        (BatchCertificate { digest, acks }, wallets)
    }

    #[test]
    fn priority_order_is_deterministic_across_calls() {
        let (cert, _) = cert_with_n_signers([1u8; 32], 6);
        let a = repair_priority(&cert, [1u8; 32]);
        let b = repair_priority(&cert, [1u8; 32]);
        assert_eq!(a, b, "the same certificate + digest must always produce the same order");
    }

    #[test]
    fn priority_order_includes_every_signer_exactly_once() {
        let (cert, wallets) = cert_with_n_signers([2u8; 32], 5);
        let order = repair_priority(&cert, [2u8; 32]);
        assert_eq!(order.len(), 5);
        let mut sorted_order = order.clone();
        sorted_order.sort();
        let mut sorted_wallets = wallets.clone();
        sorted_wallets.sort();
        assert_eq!(sorted_order, sorted_wallets, "every signer must appear exactly once, none invented, none dropped");
    }

    #[test]
    fn priority_order_differs_across_different_batches() {
        // Same signer SET, different batch digest -> the doc's claim that
        // "repeated repairs across many batches spread load" requires the
        // order to actually depend on the digest, not just the validator set.
        let (cert, _) = cert_with_n_signers([3u8; 32], 8);
        let order_a = repair_priority(&cert, [0xAAu8; 32]);
        let order_b = repair_priority(&cert, [0xBBu8; 32]);
        assert_ne!(order_a, order_b, "different batch digests should (almost always) reorder an 8-signer set differently");
    }

    #[test]
    fn next_repair_peer_skips_already_tried() {
        let (cert, _) = cert_with_n_signers([4u8; 32], 4);
        let digest = [4u8; 32];
        let order = repair_priority(&cert, digest);
        let first = next_repair_peer(&cert, digest, &[]).unwrap();
        assert_eq!(first, order[0]);
        let second = next_repair_peer(&cert, digest, &[first]).unwrap();
        assert_eq!(second, order[1]);
        assert_ne!(first, second);
    }

    #[test]
    fn next_repair_peer_returns_none_once_everyone_tried() {
        let (cert, wallets) = cert_with_n_signers([5u8; 32], 3);
        let digest = [5u8; 32];
        assert!(next_repair_peer(&cert, digest, &wallets).is_none(), "once every signer has been tried, there is no one left to ask");
    }
}
