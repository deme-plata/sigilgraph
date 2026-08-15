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

    /// BLAKE3 over `(worker, round, txs)` — the Phase-0 identity scheme, kept
    /// for `dissemination.rs`'s existing shard/reassemble round-trip. NOT
    /// domain-separated and doesn't carry chain/epoch/sequence context —
    /// superseded by [`Self::canonical_header`] (Phase A,
    /// SIGIL_BRAIDPOOL_v1_1.md §3.4) wherever a real sealing context (a
    /// chain id, an epoch, a sequence number) is actually available. Left
    /// as-is rather than forced to fake those fields with placeholder values
    /// — that context doesn't exist yet in this crate (it's Phase C's
    /// "local batches", not Phase A's).
    pub fn digest(&self) -> [u8; 32] {
        let bytes = self.encode();
        blake3::hash(&bytes).into()
    }

    /// Build the Phase-A canonical header (`crate::canonical::BatchHeaderV1`)
    /// for this batch, given the sealing context this type doesn't itself
    /// carry. `tx_root` is a REAL Merkle root over each tx's own canonical
    /// hash (`crate::merkle::merkle_root`), not a bare concatenation hash —
    /// see canonical.rs's doc comment for why that distinction matters
    /// (reordering detection, no leaf/node hash collision).
    pub fn canonical_header(
        &self,
        chain_id: [u8; 32],
        epoch: u64,
        sequence: u64,
        previous: Option<[u8; 32]>,
    ) -> crate::canonical::BatchHeaderV1 {
        let tx_hashes: Vec<[u8; 32]> = self.txs.iter().map(|t| t.tx.hash()).collect();
        let uncompressed_len = self.encode().len() as u32;
        crate::canonical::BatchHeaderV1::new_replicated(
            chain_id,
            epoch,
            self.worker,
            sequence,
            previous,
            &tx_hashes,
            uncompressed_len,
        )
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
///
/// BUG FIXED 2026-08-15 (caught in external review, independently re-derived
/// before accepting the fix): `2f+1` is only a safe quorum when `n` is EXACTLY
/// `3f+1`. Two quorums of size `q` out of `n` are only guaranteed to intersect
/// in more than `f` nodes (i.e. in at least one honest node) when `2q > n+f`.
/// For `n = 3f+1` that requires `q >= 2f+1`, which is where `2f+1` came from —
/// but for `n` NOT of that exact form (e.g. n=5,6,8,9,11,12 with `f =
/// floor((n-1)/3)`), `2f+1` falls short of `2q > n+f` and quorum intersection
/// is NOT guaranteed: two disjoint-enough "quorums" could both certify,
/// including one assembled entirely from Byzantine signers. Verified by
/// brute-force check for n=1..12: `2f+1` fails the intersection property at 7
/// of those 12 values. `n-f` always satisfies `2q>n+f` for `f=floor((n-1)/3)`
/// (sometimes more conservative than the tight minimum, never unsafe) — this
/// is the standard PBFT-family quorum size for a validator set that isn't
/// necessarily exactly `3f+1`. The existing test suite did not catch this: it
/// only exercised n=4,7,10, all exactly `3f+1`, where the two formulas happen
/// to coincide — see `quorum_threshold_matches_2f_plus_1_when_n_is_3f_plus_1`
/// and the new `quorum_threshold_safe_for_non_3f_plus_1_n` test below.
pub fn quorum_threshold(n: usize) -> usize {
    if n <= 1 {
        return n.max(1);
    }
    let f = (n - 1) / 3;
    n - f
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
    /// Domain-separated identity for the certificate itself (distinct from
    /// `self.digest`, which identifies the BATCH the certificate is ABOUT).
    /// Used by `BatchRefV1::certificate_hash` (Phase F, §12) so a block can
    /// commit to "this exact quorum of acks proved availability," not just
    /// "some certificate for this batch existed" — a certificate with a
    /// DIFFERENT ack set for the same batch (e.g. after new acks arrive)
    /// gets a different hash, so a block ref stays pinned to the specific
    /// evidence that was actually verified when the block was produced.
    pub fn hash(&self) -> [u8; 32] {
        let bytes = crate::canonical::canonical_encode(self);
        let mut h = blake3::Hasher::new();
        h.update(b"SIGIL/CERTIFICATE/V1");
        h.update(&bytes);
        h.finalize().into()
    }

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
    fn quorum_threshold_matches_2f_plus_1_when_n_is_exactly_3f_plus_1() {
        // At n = 3f+1 EXACTLY, n-f and 2f+1 coincide (n-f = 3f+1-f = 2f+1) —
        // this is the classic textbook case and the ONLY case the original
        // (buggy) `2f+1` formula happened to get right.
        // n=4 -> f=1 -> n-f=3 (tolerate 1 fault out of 4)
        assert_eq!(quorum_threshold(4), 3);
        // n=7 -> f=2 -> n-f=5
        assert_eq!(quorum_threshold(7), 5);
        // n=10 -> f=3 -> n-f=7
        assert_eq!(quorum_threshold(10), 7);
    }

    /// The bug this guards against: for n NOT of the form 3f+1, `2f+1` (the
    /// original formula) falls short of the minimum quorum size needed to
    /// guarantee two quorums intersect in an honest node (`2q > n+f`). This
    /// test would have FAILED against the pre-fix `2f+1` implementation —
    /// it exists specifically because the OTHER quorum test above didn't
    /// catch the bug (all three of its n values happen to be exactly 3f+1).
    #[test]
    fn quorum_threshold_safe_for_non_3f_plus_1_n() {
        // n=5: f=1, 2f+1=3 (UNSAFE: needs q>=4), n-f=4 (safe)
        assert_eq!(quorum_threshold(5), 4);
        // n=6: f=1, 2f+1=3 (UNSAFE: needs q>=4), n-f=5 (safe, conservative)
        assert_eq!(quorum_threshold(6), 5);
        // n=8: f=2, 2f+1=5 (UNSAFE: needs q>=6), n-f=6 (safe)
        assert_eq!(quorum_threshold(8), 6);
        for n in 1..=64usize {
            let f = if n <= 1 { 0 } else { (n - 1) / 3 };
            let q = quorum_threshold(n);
            // 2q > n+f is the quorum-intersection safety property; check it
            // holds for every n in range, not just the hand-picked examples.
            assert!(2 * q > n + f, "quorum_threshold({n})={q} fails 2q>n+f (f={f})");
        }
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
    fn canonical_header_matches_worker_batch_content() {
        let b = WorkerBatch::new(WorkerId(2), 1, vec![dummy_tx(0), dummy_tx(1)]);
        let h = b.canonical_header([5u8; 32], 3, 10, None);
        assert_eq!(h.worker, WorkerId(2), "header must carry the batch's actual worker id");
        assert_eq!(h.tx_count, 2);
        let expected_root = crate::merkle::merkle_root(
            &b.txs.iter().map(|t| t.tx.hash()).collect::<Vec<_>>(),
        );
        assert_eq!(h.tx_root, expected_root, "canonical_header must use a REAL Merkle root, not WorkerBatch::digest()'s bare hash");
    }

    #[test]
    fn canonical_header_id_differs_from_legacy_digest() {
        // The two identity schemes are deliberately different (see the doc
        // comment on WorkerBatch::digest) — this pins that they don't
        // accidentally collide, which would be confusing if anyone ever
        // compared the two.
        let b = WorkerBatch::new(WorkerId(0), 1, vec![dummy_tx(0)]);
        let legacy = b.digest();
        let canonical = b.canonical_header([0u8; 32], 0, 0, None).batch_id();
        assert_ne!(legacy, canonical);
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
    fn certificate_hash_changes_when_ack_set_changes() {
        let digest = [3u8; 32];
        let (sk1, pk1, _) = ed25519_keygen();
        let (sk2, pk2, _) = ed25519_keygen();
        let cert_a = BatchCertificate::try_certify(digest, vec![BatchAck::sign(digest, &sk1, &pk1)], 1).unwrap();
        let cert_b = BatchCertificate::try_certify(
            digest,
            vec![BatchAck::sign(digest, &sk1, &pk1), BatchAck::sign(digest, &sk2, &pk2)],
            1,
        )
        .unwrap();
        assert_ne!(
            cert_a.hash(), cert_b.hash(),
            "a certificate's hash must reflect exactly which acks it holds, not just which batch it's about"
        );
        assert_ne!(cert_a.hash(), cert_a.digest, "certificate hash and the batch digest it's about must be distinct identities");
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
