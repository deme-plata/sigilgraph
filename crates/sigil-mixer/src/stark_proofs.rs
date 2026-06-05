//! Real STARK proofs via flux-zk-stark::WalletPrivacyStarkProver.
//!
//! Phase 1 of sigil-mixer — replaces verify::verify_zk_stub with actual
//! post-quantum ZK proving + verification. Only compiled with the
//! `real-zk` cargo feature so prototyping callers (sigil-tx integration
//! before flux-zk-stark is wired in) don't pay the ~30s cold compile
//! cost from the arkworks-stark dep chain.
//!
//! flux-zk-stark's API operates on cleartext (sender, receiver, amount,
//! balance) tuples — the prover is the wallet that knows everything; the
//! proof bytes are what go on the wire. The verifier sees only the proof +
//! tx_commitment + nullifier (which IS the ShieldedSend wire shape).
//!
//! Single-input single-output limitation: this Phase 1 cut handles the
//! common case (one wallet sending one amount to one recipient, plus
//! implicit change). Multi-input multi-output ShieldedSend (where the
//! sender consolidates multiple commitments) needs a custom Merkle-proof
//! aggregation circuit that flux-zk-stark doesn't expose yet — that's
//! Phase 2 of sigil-mixer.
//!
//! Async: flux-zk-stark::WalletPrivacyStarkProver is async (tokio-based).
//! Callers must run inside a tokio runtime. sigil-tx today is sync; we
//! expect the integration to use `tokio::runtime::Handle::current().block_on(...)`
//! initially, with full async migration on a separate task.

use anyhow::Result;
use flux_zk_stark::wallet_privacy_stark::{
    StarkTransactionPrivacyProof, WalletPrivacyStarkProver,
};

/// Generate a real STARK proof for a single-input single-output ShieldedSend.
///
/// Caller (the wallet) supplies cleartext (sender, receiver, amount,
/// balance). Output proof is what goes on the wire — opaque to anyone
/// without the cleartext.
///
/// `enable_gpu`: pass `true` if the host has a CUDA-capable GPU and the
/// flux-zk-stark build included gpu-acceleration. Otherwise false — CPU
/// proving works on all hosts, just slower.
pub async fn prove_shielded_send_stark(
    sender_address: &[u8; 32],
    receiver_address: &[u8; 32],
    amount: u64,
    sender_balance: u64,
    enable_gpu: bool,
) -> Result<StarkTransactionPrivacyProof> {
    let mut prover = WalletPrivacyStarkProver::new(enable_gpu).await?;
    prover
        .prove_transaction_privacy_stark(
            sender_address,
            receiver_address,
            amount,
            sender_balance,
        )
        .await
}

/// Verify a real STARK proof. Returns `true` if the proof is valid for the
/// embedded tx_commitment + nullifier. Returns `false` for invalid proofs.
/// Returns `Err` only on infrastructure failure (e.g. malformed bytes).
pub async fn verify_shielded_send_stark(
    proof: &StarkTransactionPrivacyProof,
    enable_gpu: bool,
) -> Result<bool> {
    let mut prover = WalletPrivacyStarkProver::new(enable_gpu).await?;
    prover.verify_transaction_privacy_stark(proof).await
}

/// Re-export so callers don't need to know the flux-zk-stark module path.
pub use flux_zk_stark::wallet_privacy_stark::StarkTransactionPrivacyProof as StarkProof;
