//! PV-0 fail-closed contract, exercised from OUTSIDE the crate.
//!
//! The unit tests in `src/verify.rs` can see module-private items; an
//! external integration test sees exactly what a settlement chokepoint in
//! sigil-state would see. That is the surface the 2026-08-19 PV-0 audit was
//! worried about, so it gets its own test binary.
//!
//! Contract under test:
//!   1. `verify_shielded_send` never yields `VerifyOk` — in either feature
//!      configuration, for any well-formed tx.
//!   2. Only `verify_shielded_send_with_stark` (real-zk) can, and only after
//!      flux-zk-stark actually verified a proof that is bound to the tx.
//!
//! Note there is also a compile-time half of the contract that no runtime
//! test can express: `VerifyOk`'s fields are private and its constructor is
//! `#[cfg(feature = "real-zk")]` + module-private, so an external crate
//! cannot fabricate one. Rewriting `let ok = VerifyOk { n_inputs: 1, .. };`
//! here would not compile — that is the point.

use sigil_mixer::tx::PrivacyProofBundle;
use sigil_mixer::{
    commit, derive_nullifier, verify_shielded_send, ShieldedPool, ShieldedSendTxData,
    ShieldedSendVerifyError,
};

fn token() -> [u8; 32] {
    [0u8; 32]
}

/// A tx that passes every structural check and every nullifier check — the
/// exact shape that used to sail through with a stubbed proof.
fn well_formed_tx() -> ShieldedSendTxData {
    let c1 = commit(100, &token(), &[1u8; 32]);
    let c2 = commit(50, &token(), &[2u8; 32]);
    let mut input_nullifiers = vec![derive_nullifier(c1, b"alice"), derive_nullifier(c2, b"alice")];
    input_nullifiers.sort();
    let mut output_commitments = vec![
        commit(140, &token(), &[10u8; 32]),
        commit(9, &token(), &[11u8; 32]),
    ];
    output_commitments.sort();
    ShieldedSendTxData {
        input_nullifiers,
        output_commitments,
        fee: 1,
        token_hint: token(),
        proof: PrivacyProofBundle {
            system: "groth16".to_string(),
            proof_bytes: vec![1u8; 192],
            commitment_set_root: [0u8; 32],
        },
    }
}

#[test]
fn external_caller_never_gets_a_verify_ok_from_the_sync_path() {
    let pool = ShieldedPool::new();
    let err = verify_shielded_send(&well_formed_tx(), &pool)
        .expect_err("sync verify must fail closed");

    #[cfg(not(feature = "real-zk"))]
    assert!(
        matches!(err, ShieldedSendVerifyError::ZkStubAccepted),
        "default build must reject with ZkStubAccepted, got {err:?}"
    );
    #[cfg(feature = "real-zk")]
    assert!(
        matches!(err, ShieldedSendVerifyError::ZkVerify(_)),
        "real-zk sync path must still reject, got {err:?}"
    );
}

/// Sweeping the wire fields a producer controls must not find a shape that
/// gets waved through. Fees, token hints and proof-system tags are all
/// attacker-chosen; none of them may unlock a VerifyOk.
#[test]
fn no_producer_controlled_field_unlocks_verification() {
    let pool = ShieldedPool::new();
    for system in ["groth16", "plonk", "stark", "", "REAL", "flux-zk-stark"] {
        for fee in [0u128, 1, u128::MAX] {
            for hint in [[0u8; 32], [0xFFu8; 32]] {
                let mut tx = well_formed_tx();
                tx.proof.system = system.to_string();
                tx.fee = fee;
                tx.token_hint = hint;
                assert!(
                    verify_shielded_send(&tx, &pool).is_err(),
                    "sync path accepted system={system:?} fee={fee}"
                );
            }
        }
    }
}

/// Cheap, specific errors must still win, so a rejected tx still tells the
/// operator *why*. A fail-closed path that collapses every failure into one
/// error is a debugging trap.
#[test]
fn specific_errors_are_still_reported_ahead_of_the_zk_rejection() {
    let mut pool = ShieldedPool::new();
    let tx = well_formed_tx();
    pool.spend(tx.input_nullifiers[0]).unwrap();

    assert!(matches!(
        verify_shielded_send(&tx, &pool).unwrap_err(),
        ShieldedSendVerifyError::DoubleSpend(_)
    ));

    let mut empty_inputs = tx.clone();
    empty_inputs.input_nullifiers.clear();
    assert!(matches!(
        verify_shielded_send(&empty_inputs, &ShieldedPool::new()).unwrap_err(),
        ShieldedSendVerifyError::TxData(_)
    ));
}

#[cfg(feature = "real-zk")]
mod real_zk {
    use super::*;
    use sigil_mixer::{
        prove_shielded_send_stark, verify_shielded_send_with_stark, Nullifier,
    };

    /// Build the 1-in/1-out tx that a real STARK proof actually covers, with
    /// every binding field derived from the proof itself.
    fn bound_tx(proof: &sigil_mixer::StarkProof) -> ShieldedSendTxData {
        ShieldedSendTxData {
            input_nullifiers: vec![Nullifier(proof.nullifier)],
            output_commitments: vec![commit(100, &token(), &[7u8; 32])],
            fee: 0,
            token_hint: token(),
            proof: PrivacyProofBundle {
                system: "stark".to_string(),
                proof_bytes: proof.stark_proof.clone(),
                commitment_set_root: [0u8; 32],
            },
        }
    }

    #[tokio::test]
    async fn genuine_stark_path_is_the_only_source_of_verify_ok() {
        let sender = [0xDEu8; 32];
        let receiver = [0x01u8; 32];
        let proof = prove_shielded_send_stark(&sender, &receiver, 100, 1_000_000, false)
            .await
            .expect("prove");

        let tx = bound_tx(&proof);
        let pool = ShieldedPool::new();

        // The sync entry point stays closed even with real-zk compiled in.
        assert!(verify_shielded_send(&tx, &pool).is_err());

        // The genuine, bound, verified path is the one that opens.
        let ok = verify_shielded_send_with_stark(&tx, &pool, &proof, false)
            .await
            .expect("bound + valid proof must verify");
        assert_eq!(ok.n_inputs(), 1);
        assert_eq!(ok.n_outputs(), 1);
        assert_eq!(ok.fee(), 0);
    }

    #[tokio::test]
    async fn unbound_or_replayed_proofs_are_rejected() {
        let proof = prove_shielded_send_stark(&[0xDEu8; 32], &[0x01u8; 32], 100, 1_000_000, false)
            .await
            .expect("prove");
        let pool = ShieldedPool::new();
        let tx = bound_tx(&proof);

        // Wire bytes no longer are the proof body being verified — this is the
        // replay route: verify proof A while the tx carries proof B.
        let mut swapped = tx.clone();
        swapped.proof.proof_bytes = vec![0xAAu8; proof.stark_proof.len()];
        assert!(matches!(
            verify_shielded_send_with_stark(&swapped, &pool, &proof, false)
                .await
                .unwrap_err(),
            ShieldedSendVerifyError::ZkProofNotBound(_)
        ));

        // The proof is about a different spend.
        let mut wrong_nullifier = tx.clone();
        wrong_nullifier.input_nullifiers = vec![Nullifier([0x55u8; 32])];
        assert!(matches!(
            verify_shielded_send_with_stark(&wrong_nullifier, &pool, &proof, false)
                .await
                .unwrap_err(),
            ShieldedSendVerifyError::ZkProofNotBound(_)
        ));

        // Wrong system tag.
        let mut wrong_system = tx.clone();
        wrong_system.proof.system = "groth16".to_string();
        assert!(matches!(
            verify_shielded_send_with_stark(&wrong_system, &pool, &proof, false)
                .await
                .unwrap_err(),
            ShieldedSendVerifyError::ZkProofNotBound(_)
        ));

        // Arity the circuit does not cover.
        let mut two_out = tx.clone();
        two_out.output_commitments = vec![
            commit(60, &token(), &[7u8; 32]),
            commit(40, &token(), &[8u8; 32]),
        ];
        two_out.output_commitments.sort();
        assert!(matches!(
            verify_shielded_send_with_stark(&two_out, &pool, &proof, false)
                .await
                .unwrap_err(),
            ShieldedSendVerifyError::ZkProofNotBound(_)
        ));

        // Double-spend still outranks everything.
        let mut spent = ShieldedPool::new();
        spent.spend(tx.input_nullifiers[0]).unwrap();
        assert!(matches!(
            verify_shielded_send_with_stark(&tx, &spent, &proof, false)
                .await
                .unwrap_err(),
            ShieldedSendVerifyError::DoubleSpend(_)
        ));
    }

    /// A bound-but-invalid proof must not verify: the binding checks are a
    /// gate in front of the verifier, not a replacement for it.
    #[tokio::test]
    async fn bound_but_invalid_proof_still_fails() {
        let good = prove_shielded_send_stark(&[0xDEu8; 32], &[0x01u8; 32], 50, 200, false)
            .await
            .expect("prove");

        let mut tampered = good.clone();
        tampered.stark_proof = vec![0xAAu8; good.stark_proof.len()];

        // Rebuild the tx so the wire bytes DO match the tampered proof —
        // every binding check passes, only the cryptography fails.
        let tx = bound_tx(&tampered);
        let pool = ShieldedPool::new();

        assert!(
            verify_shielded_send_with_stark(&tx, &pool, &tampered, false)
                .await
                .is_err(),
            "a garbage proof body must not produce VerifyOk"
        );
    }
}
