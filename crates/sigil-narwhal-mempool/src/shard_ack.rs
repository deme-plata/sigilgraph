//! shard_ack.rs — closes the SIGIL_BRAIDPOOL_v1_1.md §3.5/§11 gap: binds
//! `shard_index`/`shard_hash` INSIDE a validator's signed acknowledgement,
//! instead of signing only a whole-batch digest. `types::BatchAck` (Phase A)
//! signs `(digest, validator)` only — it can prove "I hold *something* under
//! this batch's commitment," never "I hold shard `i` specifically." Under
//! Reed-Solomon dissemination, different validators genuinely hold different
//! bytes, so that's not a cosmetic gap: nothing stops a validator from
//! signing an ack for a shard it never received, or replaying its own ack's
//! signature bytes as if they covered a different shard.
//!
//! This is Phase E's explicitly tracked follow-up, quoted verbatim from
//! `dissemination.rs`'s own module doc comment:
//!
//! > "Known gap, stated plainly: the design doc's §3.5/§11 correction ...
//! > binds shard_index into the signed ack so a real deployment knows
//! > exactly which shard each signer holds. types::BatchAck was never
//! > updated to that shape ... closing that gap is tracked follow-up work
//! > (a bigger, separately-reviewable change to an already-shipped,
//! > already-tested signed message), not done in this pass."
//!
//! `types::BatchAck`/`BatchCertificate` are left exactly as they are —
//! `availability_testnet.rs`'s Phase D simulation exercises REPLICATED
//! availability, where every validator holds an identical full copy and
//! there is exactly one "shard" (the whole batch; see `canonical.rs`'s doc
//! comment on `shard_root`). That mode has nothing to bind a shard_index
//! TO. This module is specifically for the Reed-Solomon path.
//!
//! **What this module does NOT do, stated as plainly as every other phase's
//! boundary:** it does not compute a real per-shard Merkle tree or Merkle
//! inclusion proofs for `shard_root` — `dissemination.rs`'s `shard_batch`
//! still RS-codes the whole `(header, batch)` pair as one blob and the
//! header's `shard_root` still equals `tx_root` (Replicated-mode's only
//! constructor). `shard_hash` here is the hash of the SHARD BYTES a
//! validator was actually sent, bound into its signature — genuine progress
//! (a lying validator can no longer sign an ack for content it never held,
//! without being caught the moment anyone checks the hash against the bytes
//! they hold), but "wrong Merkle proof rejected" (§21's own test list) needs
//! that separate shard-Merkle-tree piece, which is not built here.
//!
//! Not wired into `sigil-node`, `sigil-api`, or anything on the live
//! producer path — same standalone-and-inert status as every other
//! BraidPool phase piece. SIGIL is a genuine n=1 network today
//! (`crate::body_mode::SIGIL_CURRENT_VALIDATOR_COUNT`); this machinery is
//! architecturally inert until `crate::body_mode`'s n>=4 BFT floor is real.

use serde::{Deserialize, Serialize};
use sigil_state::WalletId;

use crate::canonical::{canonical_encode, CodingProfile};
use crate::types::{quorum_threshold, WorkerId};

/// The shared, per-batch fields every ack for one batch is signed alongside.
/// NOT itself a signed object — matches SIGIL_BRAIDPOOL_v1_1.md §11's
/// `BatchStatementV1`: exists so a certificate doesn't repeat these fields
/// once per validator, and so each ack's signing bytes are unambiguous about
/// which batch (chain/epoch/worker/sequence/coding), not just which digest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchStatementV1 {
    pub chain_id: [u8; 32],
    pub epoch: u64,
    pub worker: WorkerId,
    pub sequence: u64,
    pub batch_id: [u8; 32],
    pub shard_root: [u8; 32],
    pub coding: CodingProfile,
}

/// Deterministic validator -> shard mapping for ONE batch, so a verifier
/// never has to trust a transmitted `shard_index` alone (§3.5: "do both:
/// sign it AND recompute it — never trust the transmitted field"). Mirrors
/// `worker.rs::epoch_salted_index`'s exact construction (`BLAKE3(domain ||
/// salt || wallet) mod count`), salted by the batch's own id rather than an
/// epoch seed — assignment is scoped to THIS batch's shard set, not to a
/// worker-routing epoch (a batch's shard count can differ batch to batch;
/// `dissemination.rs`'s shard count is `k + parity`, chosen per call).
pub fn expected_shard_index(validator: &WalletId, batch_id: &[u8; 32], shard_count: usize) -> u16 {
    let count = shard_count.max(1);
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/SHARD-ASSIGN/V1");
    h.update(batch_id);
    h.update(validator);
    let out = h.finalize();
    let x = u64::from_le_bytes(out.as_bytes()[0..8].try_into().unwrap());
    (x as usize % count) as u16
}

/// One validator's acknowledgement that it holds shard `shard_index` (whose
/// content hashes to `shard_hash`) of the batch described by a
/// `BatchStatementV1`. `shard_index`/`shard_hash` are bound INSIDE the
/// signature — §3.5's fix. `sig` is `Vec<u8>` for the same reason
/// `types::BatchAck::sig` is: this workspace's serde doesn't have built-in
/// fixed-array impls for 64 bytes, and the established local convention
/// (`sigil-header::SignatureBytes`, `sigil_tx::SignedTx`) is a Vec-backed
/// signature field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchAckV1 {
    pub validator: WalletId,
    pub shard_index: u16,
    pub shard_hash: [u8; 32],
    pub sig: Vec<u8>,
}

impl BatchAckV1 {
    /// The exact bytes a validator signs: the shared per-batch statement,
    /// domain-tagged, concatenated with THIS validator's own shard_index and
    /// shard_hash — so two validators holding different shards of the same
    /// batch produce genuinely different signed messages, and a signature
    /// can never be replayed as if it attested to a different shard.
    fn signing_bytes(statement: &BatchStatementV1, shard_index: u16, shard_hash: &[u8; 32]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64 + 32 + 2 + 32);
        buf.extend_from_slice(b"SIGIL/BATCHACK/V1");
        buf.extend_from_slice(&canonical_encode(statement));
        buf.extend_from_slice(&shard_index.to_le_bytes());
        buf.extend_from_slice(shard_hash);
        buf
    }

    pub fn sign(
        statement: &BatchStatementV1,
        shard_index: u16,
        shard_hash: [u8; 32],
        sk_bytes: &[u8; 32],
        pk_bytes: &[u8; 32],
    ) -> Self {
        use ed25519_dalek::{Signer, SigningKey};
        let validator = sigil_tx::wallet_id_from_pubkey(pk_bytes);
        let sk = SigningKey::from_bytes(sk_bytes);
        let msg = Self::signing_bytes(statement, shard_index, &shard_hash);
        let sig = sk.sign(&msg).to_bytes().to_vec();
        Self { validator, shard_index, shard_hash, sig }
    }

    /// Signature check ONLY — confirms the ack's fields are self-consistent
    /// and genuinely signed by `pk_bytes`, but NOT that `shard_index` is the
    /// shard this validator was actually assigned (a validator signs its own
    /// claim; nothing here checks the claim is the honest one). Use
    /// [`Self::verify_assigned`] for the full §3.5 check.
    pub fn verify_signature(&self, statement: &BatchStatementV1, pk_bytes: &[u8; 32]) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let Ok(vk) = VerifyingKey::from_bytes(pk_bytes) else { return false };
        let Ok(sig_bytes): Result<[u8; 64], _> = self.sig.as_slice().try_into() else { return false };
        let sig = Signature::from_bytes(&sig_bytes);
        let msg = Self::signing_bytes(statement, self.shard_index, &self.shard_hash);
        vk.verify(&msg, &sig).is_ok()
    }

    /// The full §3.5 check: the signature verifies over the transmitted
    /// shard_index/shard_hash AND the transmitted shard_index matches the
    /// deterministic assignment independently recomputed for this validator
    /// — never trust the transmitted index alone. A validator can sign a
    /// perfectly valid ack for the WRONG shard (e.g. one it obtained from
    /// another validator instead of the one it was actually assigned); this
    /// is the check that catches that, which `verify_signature` alone can't.
    pub fn verify_assigned(&self, statement: &BatchStatementV1, pk_bytes: &[u8; 32], shard_count: usize) -> bool {
        self.verify_signature(statement, pk_bytes)
            && expected_shard_index(&self.validator, &statement.batch_id, shard_count) == self.shard_index
    }
}

/// A shard-availability certificate: the batch-level statement plus a
/// verified quorum of per-shard acks (SIGIL_BRAIDPOOL_v1_1.md §11). Unlike
/// `types::BatchCertificate::try_certify` — which does NOT check ack
/// signatures itself, a documented Phase D finding (see
/// `availability_testnet.rs`'s module doc comment and
/// `try_certify_alone_does_not_check_signatures_verification_is_the_callers_job`)
/// — `AvailabilityCertificateV1::try_certify` verifies every ack itself
/// (signature AND shard assignment) before counting it toward quorum,
/// closing that gap in the new type rather than reproducing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityCertificateV1 {
    pub statement: BatchStatementV1,
    pub acks: Vec<BatchAckV1>,
}

impl AvailabilityCertificateV1 {
    /// Domain-separated identity for the certificate itself, distinct from
    /// `statement.batch_id` (which identifies the BATCH, not this specific
    /// quorum of acks) — same reasoning as `types::BatchCertificate::hash`.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"SIGIL/SHARD-CERTIFICATE/V1");
        h.update(&canonical_encode(self));
        h.finalize().into()
    }

    /// Verify + assemble in one pass: dedups by validator, keeps only acks
    /// whose signature verifies against a real committee member's pubkey
    /// (`pk_for` — returning `None` for a non-member rejects it, the §11
    /// membership check) AND whose shard_index matches the deterministic
    /// assignment for `shard_count` — then requires `quorum_threshold(n)`
    /// distinct such acks, exactly as `types::BatchCertificate::try_certify`
    /// requires for whole-batch acks.
    pub fn try_certify(
        statement: BatchStatementV1,
        acks: Vec<BatchAckV1>,
        n: usize,
        shard_count: usize,
        pk_for: impl Fn(&WalletId) -> Option<[u8; 32]>,
    ) -> Option<Self> {
        let mut seen = std::collections::HashSet::new();
        let valid: Vec<BatchAckV1> = acks
            .into_iter()
            .filter(|a| seen.insert(a.validator))
            .filter(|a| pk_for(&a.validator).is_some_and(|pk| a.verify_assigned(&statement, &pk, shard_count)))
            .collect();
        if valid.len() >= quorum_threshold(n) {
            Some(Self { statement, acks: valid })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::CodingProfile;
    use crate::types::WorkerId;

    fn statement(batch_id: [u8; 32]) -> BatchStatementV1 {
        BatchStatementV1 {
            chain_id: [1u8; 32],
            epoch: 3,
            worker: WorkerId(0),
            sequence: 7,
            batch_id,
            shard_root: [2u8; 32],
            coding: CodingProfile::ReedSolomon { data_shards: 8, parity_shards: 4 },
        }
    }

    #[test]
    fn expected_shard_index_is_deterministic() {
        let validator = [9u8; 32];
        let batch_id = [5u8; 32];
        assert_eq!(
            expected_shard_index(&validator, &batch_id, 12),
            expected_shard_index(&validator, &batch_id, 12),
            "same inputs must always produce the same assignment"
        );
    }

    #[test]
    fn expected_shard_index_is_batch_scoped_not_fixed_per_validator() {
        // A validator's assignment must be free to differ between batches —
        // otherwise a validator would always hold "shard 0" (say) for every
        // batch forever, defeating the point of spreading load/targeting risk
        // across the shard set batch by batch.
        let validator = [9u8; 32];
        let mut saw_different = false;
        for i in 0..32u8 {
            let batch_id = [i; 32];
            if expected_shard_index(&validator, &batch_id, 12) != expected_shard_index(&validator, &[0u8; 32], 12) {
                saw_different = true;
                break;
            }
        }
        assert!(saw_different, "assignment must vary across at least some batches for a fixed validator");
    }

    #[test]
    fn expected_shard_index_always_in_range() {
        for i in 0..64u8 {
            let idx = expected_shard_index(&[i; 32], &[7u8; 32], 12);
            assert!((idx as usize) < 12, "assignment must never point outside the shard set");
        }
    }

    #[test]
    fn ack_sign_and_verify_signature_roundtrip() {
        let (sk, pk, _wallet) = sigil_tx::ed25519_keygen();
        let st = statement([1u8; 32]);
        let shard_hash = [3u8; 32];
        let ack = BatchAckV1::sign(&st, 2, shard_hash, &sk, &pk);
        assert!(ack.verify_signature(&st, &pk), "ack must verify against its own signer's pubkey");
        let (_sk2, pk2, _w2) = sigil_tx::ed25519_keygen();
        assert!(!ack.verify_signature(&st, &pk2), "ack must NOT verify against a different key");
    }

    /// The exact vulnerability this module exists to close: with the OLD
    /// `types::BatchAck`, shard_index lived OUTSIDE the signed content, so
    /// tampering with it after signing wouldn't break the signature. Here it
    /// must.
    #[test]
    fn ack_rejects_tampered_shard_index() {
        let (sk, pk, _wallet) = sigil_tx::ed25519_keygen();
        let st = statement([1u8; 32]);
        let ack = BatchAckV1::sign(&st, 2, [3u8; 32], &sk, &pk);
        let mut tampered = ack.clone();
        tampered.shard_index = 5;
        assert!(
            !tampered.verify_signature(&st, &pk),
            "changing shard_index after signing must break verification — it is bound INSIDE the signature"
        );
    }

    #[test]
    fn ack_rejects_tampered_shard_hash() {
        let (sk, pk, _wallet) = sigil_tx::ed25519_keygen();
        let st = statement([1u8; 32]);
        let ack = BatchAckV1::sign(&st, 2, [3u8; 32], &sk, &pk);
        let mut tampered = ack.clone();
        tampered.shard_hash = [0xFFu8; 32];
        assert!(!tampered.verify_signature(&st, &pk), "changing shard_hash after signing must break verification");
    }

    #[test]
    fn ack_rejects_tampered_statement() {
        let (sk, pk, _wallet) = sigil_tx::ed25519_keygen();
        let st = statement([1u8; 32]);
        let ack = BatchAckV1::sign(&st, 2, [3u8; 32], &sk, &pk);
        let mut other_st = st.clone();
        other_st.sequence += 1;
        assert!(
            !ack.verify_signature(&other_st, &pk),
            "an ack for one statement must not verify against a different statement (replay across batches)"
        );
    }

    /// The "do both" half of §3.5: a signature can be perfectly genuine and
    /// still be for the WRONG shard — a validator that (honestly or not)
    /// signs an ack for a shard index other than the one it was actually
    /// assigned. `verify_signature` alone cannot catch this; `verify_assigned`
    /// must.
    #[test]
    fn verify_assigned_rejects_correctly_signed_ack_for_unassigned_shard() {
        let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
        let batch_id = [4u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        let real_index = expected_shard_index(&wallet, &batch_id, shard_count);
        let wrong_index = if real_index == 0 { 1 } else { 0 };

        let honest = BatchAckV1::sign(&st, real_index, [1u8; 32], &sk, &pk);
        assert!(honest.verify_assigned(&st, &pk, shard_count), "ack for the correctly-assigned shard must pass");

        let dishonest = BatchAckV1::sign(&st, wrong_index, [1u8; 32], &sk, &pk);
        assert!(
            dishonest.verify_signature(&st, &pk),
            "sanity: the signature itself is genuine — the validator really did sign this"
        );
        assert!(
            !dishonest.verify_assigned(&st, &pk, shard_count),
            "a genuinely-signed ack for a shard this validator was NOT assigned must still be rejected"
        );
    }

    #[test]
    fn try_certify_requires_quorum() {
        let batch_id = [6u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        let mut acks = Vec::new();
        let mut pks = std::collections::HashMap::new();
        for _ in 0..3 {
            let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
            let idx = expected_shard_index(&wallet, &batch_id, shard_count);
            acks.push(BatchAckV1::sign(&st, idx, [2u8; 32], &sk, &pk));
            pks.insert(wallet, pk);
        }
        let pk_for = |w: &WalletId| pks.get(w).copied();

        // n=4 needs quorum 3 -> exactly 3 valid acks certifies.
        let cert = AvailabilityCertificateV1::try_certify(st.clone(), acks.clone(), 4, shard_count, pk_for);
        assert!(cert.is_some());
        assert_eq!(cert.unwrap().acks.len(), 3);

        // n=7 needs quorum 5 -> the SAME 3 acks do not certify.
        assert!(AvailabilityCertificateV1::try_certify(st, acks, 7, shard_count, pk_for).is_none());
    }

    #[test]
    fn try_certify_rejects_acks_it_cannot_verify() {
        let batch_id = [8u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        // A forged ack: real signature, but for a validator identity that
        // isn't in `pk_for`'s lookup — as if a non-member "signed" (or the
        // caller never learned the real pubkey for whatever reason).
        let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
        let idx = expected_shard_index(&wallet, &batch_id, shard_count);
        let ack = BatchAckV1::sign(&st, idx, [1u8; 32], &sk, &pk);
        let empty_pk_for = |_: &WalletId| None;
        assert!(
            AvailabilityCertificateV1::try_certify(st, vec![ack], 1, shard_count, empty_pk_for).is_none(),
            "an ack whose pubkey can't be resolved (non-member) must never count toward quorum"
        );
    }

    #[test]
    fn try_certify_rejects_ack_with_wrong_shard_assignment() {
        let batch_id = [10u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
        let real_index = expected_shard_index(&wallet, &batch_id, shard_count);
        let wrong_index = if real_index == 0 { 1 } else { 0 };
        // Genuinely signed, but for the wrong shard.
        let ack = BatchAckV1::sign(&st, wrong_index, [1u8; 32], &sk, &pk);
        let mut pks = std::collections::HashMap::new();
        pks.insert(wallet, pk);
        assert!(
            AvailabilityCertificateV1::try_certify(st, vec![ack], 1, shard_count, |w| pks.get(w).copied()).is_none(),
            "try_certify must reject an otherwise-valid ack whose shard_index doesn't match its assignment"
        );
    }

    #[test]
    fn try_certify_dedupes_repeated_acks_from_the_same_validator() {
        let batch_id = [11u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
        let idx = expected_shard_index(&wallet, &batch_id, shard_count);
        let mut pks = std::collections::HashMap::new();
        pks.insert(wallet, pk);
        let acks = vec![
            BatchAckV1::sign(&st, idx, [1u8; 32], &sk, &pk),
            BatchAckV1::sign(&st, idx, [1u8; 32], &sk, &pk), // same validator "voting" twice
        ];
        // n=1: quorum is 1, dedup means 2 copies from ONE validator still
        // only count once — but that's still enough to reach the n=1 floor.
        assert!(
            AvailabilityCertificateV1::try_certify(st.clone(), acks.clone(), 1, shard_count, |w| pks.get(w).copied())
                .is_some()
        );
        // n=4: quorum is 3 DISTINCT validators; one validator repeating never
        // manufactures quorum.
        assert!(
            AvailabilityCertificateV1::try_certify(st, acks, 4, shard_count, |w| pks.get(w).copied()).is_none()
        );
    }

    #[test]
    fn certificate_hash_changes_when_ack_set_changes() {
        let batch_id = [12u8; 32];
        let st = statement(batch_id);
        let shard_count = 12;
        let (sk1, pk1, w1) = sigil_tx::ed25519_keygen();
        let (sk2, pk2, w2) = sigil_tx::ed25519_keygen();
        let idx1 = expected_shard_index(&w1, &batch_id, shard_count);
        let idx2 = expected_shard_index(&w2, &batch_id, shard_count);
        let mut pks = std::collections::HashMap::new();
        pks.insert(w1, pk1);
        pks.insert(w2, pk2);

        let ack1 = BatchAckV1::sign(&st, idx1, [1u8; 32], &sk1, &pk1);
        let ack2 = BatchAckV1::sign(&st, idx2, [1u8; 32], &sk2, &pk2);

        let cert_a =
            AvailabilityCertificateV1::try_certify(st.clone(), vec![ack1.clone()], 1, shard_count, |w| pks.get(w).copied())
                .unwrap();
        let cert_b =
            AvailabilityCertificateV1::try_certify(st, vec![ack1, ack2], 1, shard_count, |w| pks.get(w).copied())
                .unwrap();
        assert_ne!(cert_a.hash(), cert_b.hash(), "a certificate's hash must reflect exactly which acks it holds");
    }
}
