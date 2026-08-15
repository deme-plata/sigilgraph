//! types.rs — the Narwhal-shaped primitives: worker batches, availability acks,
//! quorum certificates, and the digest-only reference a block commits to.
//!
//! See ../SIGIL_NARWHAL_MEMPOOL_v0.md §3.2 for the design this implements.

use serde::{Deserialize, Serialize};
use sigil_state::WalletId;
use sigil_tx::SignedTx;

/// Which parallel ingestion lane a batch (or a worker) belongs to. Workers are
/// independent lock domains — see [`crate::worker::ShardedMempool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkerId(pub u16);

/// A Narwhal-style dissemination batch: many senders' transactions bundled as
/// ONE unit a worker gossips, acks, and later certifies. NOT the same thing as
/// `sigil_tx::AuthorizedBatch` (one wallet's own multi-op envelope) — see the
/// design doc §2 for why the two are easy to confuse and genuinely different.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerBatch {
    pub worker: WorkerId,
    pub round: u64,
    pub txs: Vec<SignedTx>,
}

impl WorkerBatch {
    pub fn new(worker: WorkerId, round: u64, txs: Vec<SignedTx>) -> Self {
        Self { worker, round, txs }
    }

    /// BLAKE3 over the canonical encoding — the identity a certificate/ack refers to.
    pub fn digest(&self) -> [u8; 32] {
        let bytes = self.encode();
        blake3::hash(&bytes).into()
    }

    fn encode(&self) -> Vec<u8> {
        // Canonical bincode-free encoding: deterministic field order via a tuple,
        // JSON is fine at Phase-0 scale (matches SignedTx's own encode() choice
        // in sigil-tx — swaps to bincode+sigil_events together in a later phase).
        serde_json::to_vec(&(self.worker, self.round, &self.txs)).unwrap_or_default()
    }

    pub fn len(&self) -> usize { self.txs.len() }
    pub fn is_empty(&self) -> bool { self.txs.is_empty() }
}

/// One validator's signed acknowledgement that it holds (or can reconstruct —
/// see dissemination.rs) the batch identified by `digest`.
// `sig` is `Vec<u8>`, not `[u8; 64]` — this workspace's serde version only has
// built-in array (de)serialize impls up to a fixed set of small lengths (the
// established local convention, matching sigil-header::SignatureBytes, is a
// Vec-backed newtype for anything signature-shaped; see sigil-tx::SignedTx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchAck {
    pub digest: [u8; 32],
    pub validator: WalletId,
    pub sig: Vec<u8>,
}

impl BatchAck {
    /// The exact bytes a validator signs — binds digest AND validator identity,
    /// so an ack can't be replayed as if a different validator produced it.
    pub fn signing_bytes(digest: &[u8; 32], validator: &WalletId) -> [u8; 64] {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(digest);
        buf[32..].copy_from_slice(validator);
        buf
    }

    pub fn sign(digest: [u8; 32], sk_bytes: &[u8; 32], pk_bytes: &[u8; 32]) -> Self {
        use ed25519_dalek::{Signer, SigningKey};
        let validator = sigil_tx::wallet_id_from_pubkey(pk_bytes);
        let sk = SigningKey::from_bytes(sk_bytes);
        let msg = Self::signing_bytes(&digest, &validator);
        let sig = sk.sign(&msg).to_bytes().to_vec();
        Self { digest, validator, sig }
    }

    pub fn verify(&self, pk_bytes: &[u8; 32]) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let Ok(vk) = VerifyingKey::from_bytes(pk_bytes) else { return false };
        let Ok(sig_bytes): Result<[u8; 64], _> = self.sig.as_slice().try_into() else { return false };
        let sig = Signature::from_bytes(&sig_bytes);
        let msg = Self::signing_bytes(&self.digest, &self.validator);
        vk.verify(&msg, &sig).is_ok()
    }
}

/// BFT quorum size for a validator set of size `n`. Standard Narwhal/PBFT
/// threshold: with `n = 3f+1` validators, `2f+1` acks certify availability
/// (tolerates up to `f` Byzantine/offline validators).
///
/// At `n <= 1` this correctly reduces to requiring exactly the one existing
/// validator's own ack — self-certification, not an invented BFT guarantee.
/// SIGIL is a single-producer testnet today (Delta/Gamma/Beta confirmed
/// permanently gone, 2026-08-14); this function is written so the real
/// multi-validator threshold activates automatically the day that's no longer
/// true, with no call-site change needed anywhere.
pub fn quorum_threshold(n: usize) -> usize {
    if n <= 1 {
        return n.max(1);
    }
    let f = (n - 1) / 3;
    2 * f + 1
}

/// A batch that has reached quorum: proof of availability, independent of
/// whether any particular validator (including this one) still holds a full
/// copy — see dissemination.rs for the erasure-coded-shard version of "holds".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCertificate {
    pub digest: [u8; 32],
    pub acks: Vec<BatchAck>,
}

impl BatchCertificate {
    /// Does `acks` (deduped by validator) reach quorum for a validator set of
    /// size `n`? Every ack's `digest` must match `digest` — mixed-batch acks
    /// never count, even if the count would otherwise reach quorum.
    pub fn try_certify(digest: [u8; 32], acks: Vec<BatchAck>, n: usize) -> Option<Self> {
        let mut seen = std::collections::HashSet::new();
        let valid: Vec<BatchAck> = acks
            .into_iter()
            .filter(|a| a.digest == digest && seen.insert(a.validator))
            .collect();
        if valid.len() >= quorum_threshold(n) {
            Some(Self { digest, acks: valid })
        } else {
            None
        }
    }
}

/// What a block actually commits to, once batches are the unit of inclusion
/// (design doc §3.2, Phase 2): a tiny digest reference, not the raw tx list.
/// One block can carry many of these — the lever that decouples committed TPS
/// from per-block byte size.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BlockBatchRef {
    pub digest: [u8; 32],
    pub worker: WorkerId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn dummy_tx(amount: u128) -> SignedTx {
        let (sk, pk, wallet) = ed25519_keygen();
        let tx = SigilTx::Send { from: wallet, to: [7u8; 32], amount, token: [0u8; 32], fee: 0 };
        ed25519_sign_tx(tx, &sk, &pk)
    }

    #[test]
    fn quorum_threshold_degenerate_n1_is_self_cert() {
        assert_eq!(quorum_threshold(0), 1); // never called with a real empty set, but never zero either
        assert_eq!(quorum_threshold(1), 1);
    }

    #[test]
    fn quorum_threshold_matches_2f_plus_1() {
        // n=4 -> f=1 -> 2f+1=3 (classic BFT: tolerate 1 fault out of 4)
        assert_eq!(quorum_threshold(4), 3);
        // n=7 -> f=2 -> 2f+1=5
        assert_eq!(quorum_threshold(7), 5);
        // n=10 -> f=3 -> 2f+1=7
        assert_eq!(quorum_threshold(10), 7);
    }

    #[test]
    fn batch_digest_is_stable_and_content_addressed() {
        let b1 = WorkerBatch::new(WorkerId(0), 1, vec![dummy_tx(0), dummy_tx(1)]);
        let b2 = WorkerBatch::new(WorkerId(0), 1, b1.txs.clone());
        assert_eq!(b1.digest(), b2.digest(), "same content -> same digest");
        let b3 = WorkerBatch::new(WorkerId(0), 2, b1.txs.clone()); // different round
        assert_ne!(b1.digest(), b3.digest(), "different round -> different digest");
    }

    #[test]
    fn batch_ack_sign_and_verify_roundtrip() {
        let (sk, pk, _wallet) = ed25519_keygen();
        let digest = [9u8; 32];
        let ack = BatchAck::sign(digest, &sk, &pk);
        assert!(ack.verify(&pk), "ack must verify against its own signer's pubkey");
        let (_sk2, pk2, _w2) = ed25519_keygen();
        assert!(!ack.verify(&pk2), "ack must NOT verify against a different key");
    }

    #[test]
    fn batch_ack_rejects_tampered_digest() {
        let (sk, pk, _wallet) = ed25519_keygen();
        let ack = BatchAck::sign([1u8; 32], &sk, &pk);
        let mut tampered = ack.clone();
        tampered.digest = [2u8; 32];
        assert!(!tampered.verify(&pk), "changing the digest after signing must break verification");
    }

    #[test]
    fn certificate_requires_real_quorum_not_just_a_count() {
        let digest = [3u8; 32];
        let mut acks = Vec::new();
        let mut pks = Vec::new();
        for _ in 0..3 {
            let (sk, pk, _w) = ed25519_keygen();
            acks.push(BatchAck::sign(digest, &sk, &pk));
            pks.push(pk);
        }
        // n=4 needs quorum 3 -> exactly 3 distinct acks certifies.
        let cert = BatchCertificate::try_certify(digest, acks.clone(), 4);
        assert!(cert.is_some());
        assert_eq!(cert.unwrap().acks.len(), 3);

        // n=7 needs quorum 5 -> the SAME 3 acks do not certify.
        assert!(BatchCertificate::try_certify(digest, acks, 7).is_none());
    }

    #[test]
    fn certificate_dedupes_repeated_acks_from_the_same_validator() {
        let digest = [4u8; 32];
        let (sk, pk, _w) = ed25519_keygen();
        let one_validator_acks = vec![
            BatchAck::sign(digest, &sk, &pk),
            BatchAck::sign(digest, &sk, &pk), // same validator "voting" twice
            BatchAck::sign(digest, &sk, &pk),
        ];
        // n=1: quorum is 1, and dedup means 3 copies from ONE validator still
        // only count once — but that's still enough to reach the n=1 threshold.
        assert!(BatchCertificate::try_certify(digest, one_validator_acks.clone(), 1).is_some());
        // n=4: quorum is 3 DISTINCT validators; one validator repeating never
        // manufactures quorum, even if the raw ack count superficially reaches 3.
        assert!(BatchCertificate::try_certify(digest, one_validator_acks, 4).is_none());
    }

    #[test]
    fn certificate_rejects_acks_for_a_different_digest() {
        let digest = [5u8; 32];
        let wrong_digest = [6u8; 32];
        let (sk, pk, _w) = ed25519_keygen();
        let acks = vec![BatchAck::sign(wrong_digest, &sk, &pk)];
        assert!(BatchCertificate::try_certify(digest, acks, 1).is_none());
    }
}
