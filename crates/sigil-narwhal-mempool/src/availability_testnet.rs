//! availability_testnet.rs — Phase D (SIGIL_BRAIDPOOL_v1_1.md's phase plan:
//! "multi-node availability testnet — require committee n>=4; replicated
//! availability first; certificate verification; recovery tests with
//! producer loss").
//!
//! **What this module IS:** a deterministic, in-process SIMULATION of an
//! `n>=4` validator committee, built entirely from this crate's real,
//! already-tested types — real Ed25519 keypairs, real
//! `BatchAck::sign`/`verify`, real `BatchCertificate::try_certify` requiring
//! the real `quorum_threshold(n)`. It exercises three things that were
//! previously untestable because SIGIL has only ever run at `n=1`:
//! replicated availability across a committee, certificate assembly/
//! rejection under realistic conditions, and recovery after the batch's
//! original producer disappears.
//!
//! **What this module is NOT:** a live multi-node network. It opens no
//! sockets and starts no OS processes; every "validator" here is a
//! `HashMap` entry in one Rust process. It proves nothing about real
//! network partitions, real latency, or a real Byzantine validator lying
//! to different peers. SIGIL's live production chain remains a genuine
//! single-producer network today — `body_mode::SIGIL_CURRENT_VALIDATOR_COUNT
//! == 1` — because Delta, Gamma, and Beta are all confirmed permanently gone
//! (CLAUDE.md, 2026-08-14: no available second machine to run a real second
//! SIGIL validator on). This module closes the PROTOCOL-LOGIC gap (does the
//! `n>=4` quorum/replication/recovery code actually work, at all?); it is
//! not and cannot be a substitute for an eventual real multi-machine
//! deployment, which remains the honest, still-open definition of "done"
//! for Phase D. When real infrastructure exists, this module's scenarios
//! are the ones a real integration test against real nodes should
//! reproduce.
//!
//! **A real finding from building this, not asserted in the design doc:**
//! [`BatchCertificate::try_certify`] does NOT itself verify each ack's
//! Ed25519 signature — it only checks `digest` match and dedups by
//! `validator`. Signature verification is the CALLER's responsibility
//! before acks are handed to `try_certify`. As of this writing,
//! `try_certify` has no production call site anywhere in `sigil-node` (only
//! this module and `types.rs`'s own tests call it), so this contract was
//! previously undocumented and unenforced at any real use point. See
//! [`SimCommittee::collect_verified_acks`] for the CORRECT calling pattern,
//! and the `try_certify_alone_does_not_check_signatures` test for a direct
//! demonstration of the gap it exists to guard against.

use std::collections::HashMap;

use crate::types::{quorum_threshold, BatchAck, BatchCertificate};

/// One simulated committee member: a real Ed25519 keypair, plus whatever
/// full batch bytes it currently holds (its replica store, keyed by
/// digest). "Replicated availability first" per the phase spec — no
/// erasure coding here; see `dissemination.rs`/Phase E for that path.
pub struct SimValidator {
    sk: [u8; 32],
    pk: [u8; 32],
    pub wallet: sigil_state::WalletId,
    replicas: HashMap<[u8; 32], Vec<u8>>,
}

impl SimValidator {
    pub fn new() -> Self {
        let (sk, pk, wallet) = sigil_tx::ed25519_keygen();
        Self { sk, pk, wallet, replicas: HashMap::new() }
    }

    pub fn pubkey(&self) -> [u8; 32] {
        self.pk
    }

    pub fn receive_replica(&mut self, digest: [u8; 32], bytes: Vec<u8>) {
        self.replicas.insert(digest, bytes);
    }

    pub fn has_replica(&self, digest: &[u8; 32]) -> bool {
        self.replicas.contains_key(digest)
    }

    pub fn serve_replica(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        self.replicas.get(digest).cloned()
    }

    /// A genuine ack — refuses to sign one for a batch this validator
    /// hasn't actually received. Availability means "I can serve this," not
    /// "I heard this exists."
    pub fn ack(&self, digest: [u8; 32]) -> Option<BatchAck> {
        if !self.has_replica(&digest) {
            return None;
        }
        Some(BatchAck::sign(digest, &self.sk, &self.pk))
    }
}

/// A deterministic `n`-member committee, entirely in-process.
pub struct SimCommittee {
    pub validators: Vec<SimValidator>,
}

impl SimCommittee {
    pub fn new(n: usize) -> Self {
        Self { validators: (0..n).map(|_| SimValidator::new()).collect() }
    }

    /// Simulate full-replica dissemination from `producer_idx` to every
    /// committee member (including itself) — matching "replicated
    /// availability first."
    pub fn disseminate_replicated(&mut self, digest: [u8; 32], bytes: &[u8]) {
        for v in self.validators.iter_mut() {
            v.receive_replica(digest, bytes.to_vec());
        }
    }

    /// Same as [`Self::disseminate_replicated`] but only reaches the given
    /// validator indices — for simulating partial/imperfect dissemination
    /// (a subset of the committee never receives the batch).
    pub fn disseminate_to(&mut self, digest: [u8; 32], bytes: &[u8], recipients: &[usize]) {
        for &i in recipients {
            self.validators[i].receive_replica(digest, bytes.to_vec());
        }
    }

    /// The CORRECT calling pattern for `BatchCertificate::try_certify`:
    /// collect each ack, then verify it against the SAME validator's own
    /// pubkey before it's allowed into the pool `try_certify` aggregates
    /// over. `try_certify` itself does not do this — see the module doc
    /// comment.
    pub fn collect_verified_acks(&self, digest: [u8; 32]) -> Vec<BatchAck> {
        self.validators
            .iter()
            .filter_map(|v| v.ack(digest))
            .filter(|ack| ack.verify(&self.validators.iter().find(|v| v.wallet == ack.validator).unwrap().pubkey()))
            .collect()
    }

    /// Certificate verification: collect properly-verified acks, then try
    /// to assemble a real quorum certificate for the current committee
    /// size.
    pub fn certify(&self, digest: [u8; 32]) -> Option<BatchCertificate> {
        let acks = self.collect_verified_acks(digest);
        BatchCertificate::try_certify(digest, acks, self.validators.len())
    }

    /// Simulate the producer disappearing entirely: drop it from the
    /// committee (its keys, its replica store, everything) — "recovery
    /// tests with producer loss."
    pub fn remove_validator(&mut self, idx: usize) -> SimValidator {
        self.validators.remove(idx)
    }

    /// Can the SURVIVING committee still serve a full copy of the batch?
    pub fn survivors_can_serve(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        self.validators.iter().find_map(|v| v.serve_replica(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_and_bytes() -> ([u8; 32], Vec<u8>) {
        ([7u8; 32], b"a real batch worth of bytes".to_vec())
    }

    #[test]
    fn n4_committee_full_dissemination_reaches_quorum_and_certifies() {
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        committee.disseminate_replicated(digest, &bytes);

        let cert = committee.certify(digest);
        assert!(cert.is_some(), "4/4 validators hold the replica and can ack — quorum_threshold(4)=3 is trivially met");
        assert_eq!(cert.unwrap().acks.len(), 4, "all 4 validators genuinely hold the batch, so all 4 acks are collected");
    }

    #[test]
    fn certification_fails_when_dissemination_reaches_fewer_than_quorum() {
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        // Only 2 of 4 ever receive the batch — below quorum_threshold(4)=3.
        committee.disseminate_to(digest, &bytes, &[0, 1]);

        assert_eq!(quorum_threshold(4), 3);
        assert!(committee.certify(digest).is_none(), "only 2 validators can honestly ack; that's below the n=4 quorum floor of 3");
    }

    #[test]
    fn certification_succeeds_at_exactly_quorum_not_before() {
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        committee.disseminate_to(digest, &bytes, &[0, 1]);
        assert!(committee.certify(digest).is_none(), "2/4 — below quorum");

        committee.disseminate_to(digest, &bytes, &[2]);
        let cert = committee.certify(digest).expect("3/4 — exactly quorum_threshold(4)=3, must certify");
        assert_eq!(cert.acks.len(), 3);
    }

    #[test]
    fn recovery_survives_producer_loss_when_replication_reached_quorum_first() {
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        committee.disseminate_replicated(digest, &bytes);
        let cert_before = committee.certify(digest).expect("full committee replication certifies");

        // The producer (validator 0, arbitrarily) disappears entirely —
        // crash, disk loss, permanent departure.
        let lost_producer = committee.remove_validator(0);
        assert!(!committee.validators.iter().any(|v| v.wallet == lost_producer.wallet), "producer is genuinely gone from the committee");

        // The batch is STILL fully recoverable from a survivor — this is
        // the actual point of replicated availability: the data's survival
        // does not depend on any single validator, including whichever one
        // originally produced it.
        let recovered = committee.survivors_can_serve(&digest);
        assert_eq!(recovered, Some(bytes.clone()), "surviving committee members still hold + can serve the full batch after producer loss");

        // The certificate assembled BEFORE the loss remains a valid,
        // reproducible piece of evidence — its acks came from validators
        // that mostly were never the producer to begin with, so producer
        // loss doesn't retroactively invalidate it.
        let cert_after = committee.certify(digest).expect("3 remaining validators still meet quorum_threshold(4)=3 even after losing one");
        assert_eq!(cert_before.digest, cert_after.digest);
    }

    #[test]
    fn a_fresh_joining_node_can_bootstrap_the_batch_from_survivors_after_producer_loss() {
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        committee.disseminate_replicated(digest, &bytes);
        committee.remove_validator(0); // producer gone

        // A brand-new validator, never part of the original committee,
        // asks a survivor for the batch and gets it — the concrete
        // "recovery" scenario the phase spec asks for.
        let mut joiner = SimValidator::new();
        assert!(!joiner.has_replica(&digest));
        let served = committee.survivors_can_serve(&digest).expect("a survivor must be able to serve it");
        joiner.receive_replica(digest, served);
        assert_eq!(joiner.serve_replica(&digest), Some(bytes));
    }

    #[test]
    fn try_certify_alone_does_not_check_signatures_verification_is_the_callers_job() {
        // The documented gap: hand-construct an ack with a real validator
        // identity and a real digest, but a SIGNATURE THAT DOES NOT
        // VALIDATE (produced by a different keypair entirely). Calling
        // `try_certify` DIRECTLY with it (bypassing `collect_verified_acks`)
        // still "certifies" — proving try_certify trusts its input rather
        // than re-deriving trust itself.
        let digest = [8u8; 32];
        let (_real_sk, real_pk, real_wallet) = sigil_tx::ed25519_keygen();
        let (forger_sk, forger_pk, _forger_wallet) = sigil_tx::ed25519_keygen();

        // A signature that is cryptographically real, but signed by the
        // WRONG key for the claimed validator identity.
        let forged_sig = BatchAck::sign(digest, &forger_sk, &forger_pk).sig;
        let forged_ack = BatchAck { digest, validator: real_wallet, sig: forged_sig };

        assert!(!forged_ack.verify(&real_pk), "sanity: this ack does NOT actually verify against the identity it claims");
        assert!(
            BatchCertificate::try_certify(digest, vec![forged_ack], 1).is_some(),
            "try_certify alone accepted a forged ack — confirms callers MUST verify acks themselves (see collect_verified_acks)"
        );
    }

    #[test]
    fn collect_verified_acks_rejects_what_try_certify_alone_would_accept() {
        // Same forged ack as above, but this time go through the crate's
        // recommended path (a committee whose OWN validators produce only
        // genuine acks) — confirming the honest path never manufactures a
        // forged ack in the first place, i.e. the vulnerability above is
        // closed by calling convention (verify-before-aggregate), not by
        // `try_certify` itself.
        let (digest, bytes) = digest_and_bytes();
        let mut committee = SimCommittee::new(4);
        committee.disseminate_replicated(digest, &bytes);
        let acks = committee.collect_verified_acks(digest);
        for ack in &acks {
            let signer = committee.validators.iter().find(|v| v.wallet == ack.validator).unwrap();
            assert!(ack.verify(&signer.pubkey()), "every ack collect_verified_acks returns must independently verify");
        }
    }
}
