//! sigil-mixer — transaction privacy primitives.
//!
//! Every SIGIL transaction is private by default. This crate defines:
//!
//!   - [`Commitment`] — Pedersen commitment to (value, blinding), 32 bytes opaque
//!   - [`Nullifier`] — owner-derived spend tag, 32 bytes opaque
//!   - [`ShieldedPool`] — set of unspent commitments + spent nullifier set
//!   - [`ShieldedSendTxData`] — wire shape for a confidential send
//!   - [`verify_shielded_send`] — pool-context validation of a ShieldedSend
//!
//! Compose with `flux_zk_snark::wallet_privacy::TransactionPrivacyProof`
//! (Groth16) for proof generation and verification. flux-zk-stark integration
//! lands in P1 for the tip-proof-friendly STARK variant.
//!
//! **Phase 0 ships opaque [u8; 32] commitments.** The 32 bytes are a hash
//! of the would-be curve point; full Pedersen with real ark-bn254 G1 ops
//! lands in P1. The wire schema is stable from Phase 0 on so sigil-tx +
//! sigil-state can integrate today without churning when the math fills in.

pub mod commitment;
pub mod nullifier;
pub mod pool;
pub mod tx;
pub mod verify;

/// Real STARK proving + verification via flux-zk-stark. Only built with
/// `--features real-zk` so the Phase 0 sync path doesn't pay the arkworks
/// compile cost.
#[cfg(feature = "real-zk")]
pub mod stark_proofs;

pub use commitment::{commit, Commitment, CommitmentError};
pub use nullifier::{derive_nullifier, Nullifier};
pub use pool::{PoolError, ShieldedPool};
pub use tx::{ShieldedSendTxData, TxDataError};
pub use verify::{verify_shielded_send, ShieldedSendVerifyError};

#[cfg(feature = "real-zk")]
pub use stark_proofs::{
    prove_shielded_send_stark, verify_shielded_send_stark, StarkProof,
};
