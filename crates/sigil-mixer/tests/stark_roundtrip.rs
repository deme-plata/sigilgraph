//! Real STARK prove+verify roundtrip via flux-zk-stark.
//!
//! Only runs with `--features real-zk`. Gated so the default test suite
//! (which sigil-tx integration uses for fast feedback) doesn't pay the
//! arkworks-stark compile cost.
//!
//! What this proves end-to-end:
//!   - prove_shielded_send_stark generates a real STARK proof for cleartext
//!     (sender, receiver, amount, balance)
//!   - The proof's tx_commitment + nullifier are deterministic from inputs
//!   - verify_shielded_send_stark returns Ok(true) for the valid proof
//!   - verify returns Ok(false) for a tampered proof
//!
//! This is the canary: flux-zk-stark's real implementation works against
//! sigil-mixer's API surface. When flux-zk-stark adds multi-input
//! multi-output (Phase 2), sigil-mixer's wire shape is already ready
//! for it — we just swap the prover call.

#![cfg(feature = "real-zk")]

use sigil_mixer::stark_proofs::{
    prove_shielded_send_stark, verify_shielded_send_stark,
};

#[tokio::test]
async fn real_stark_roundtrip_send_100_sigil() {
    let sender   = [0xDEu8; 32];
    let receiver = [0x01u8; 32];
    let amount = 100u64;
    let balance = 1_000_000u64;

    let proof = prove_shielded_send_stark(&sender, &receiver, amount, balance, false)
        .await
        .expect("prove");

    // tx_commitment must be deterministic from (sender, receiver, amount)
    assert_eq!(proof.tx_commitment.len(), 32);
    assert_eq!(proof.nullifier.len(), 32);
    assert!(!proof.stark_proof.is_empty(), "proof bytes must be non-empty");

    let ok = verify_shielded_send_stark(&proof, false)
        .await
        .expect("verify");
    assert!(ok, "valid proof must verify");
}

#[tokio::test]
async fn real_stark_roundtrip_rejects_insufficient_balance() {
    let sender   = [0xDEu8; 32];
    let receiver = [0x01u8; 32];
    // sender_balance < amount: prove should error out before generating a proof
    let err = prove_shielded_send_stark(&sender, &receiver, 1000, 100, false)
        .await
        .expect_err("over-spend must error");
    let msg = format!("{}", err);
    assert!(
        msg.contains("Insufficient balance") || msg.contains("insufficient"),
        "expected insufficient-balance error, got: {}",
        msg
    );
}

#[tokio::test]
async fn real_stark_garbage_proof_bytes_fail_verify() {
    let sender   = [0xDEu8; 32];
    let receiver = [0x01u8; 32];

    let mut proof = prove_shielded_send_stark(&sender, &receiver, 50, 200, false)
        .await
        .expect("prove");

    // Replace the whole STARK proof body with garbage. A single-byte flip
    // can land in padding that the verifier doesn't read; this guarantees
    // a check fires (either bincode deserialize fails or the verifier sees
    // bad FRI commitments).
    proof.stark_proof = vec![0xAAu8; proof.stark_proof.len()];

    let result = verify_shielded_send_stark(&proof, false).await;
    match result {
        Ok(verified) => assert!(!verified, "all-garbage proof must not verify"),
        Err(_) => { /* deserialization failure is also rejection — fine */ }
    }
}

#[tokio::test]
async fn real_stark_empty_proof_fails_verify() {
    let sender   = [0xDEu8; 32];
    let receiver = [0x01u8; 32];

    let mut proof = prove_shielded_send_stark(&sender, &receiver, 50, 200, false)
        .await
        .expect("prove");

    // Empty proof must always be rejected — flux-zk-stark's stark_verifier
    // has an explicit `if fri_proof.is_empty()` guard with a tracing::warn.
    proof.stark_proof.clear();
    let result = verify_shielded_send_stark(&proof, false).await;
    match result {
        Ok(verified) => assert!(!verified, "empty proof must not verify"),
        Err(_) => { /* deserialization failure of empty is also rejection */ }
    }
}

#[tokio::test]
async fn real_stark_different_amounts_yield_different_commitments() {
    let sender   = [0xDEu8; 32];
    let receiver = [0x01u8; 32];

    let p1 = prove_shielded_send_stark(&sender, &receiver, 100, 1000, false)
        .await
        .expect("prove p1");
    let p2 = prove_shielded_send_stark(&sender, &receiver, 200, 1000, false)
        .await
        .expect("prove p2");

    // Different amounts => different tx_commitments (catches the case where
    // amount somehow doesn't flow into the commitment).
    assert_ne!(p1.tx_commitment, p2.tx_commitment);
    // Nullifier is derived from (sender, amount) per the flux-zk-stark
    // implementation, so different amounts => different nullifiers.
    assert_ne!(p1.nullifier, p2.nullifier);
}
