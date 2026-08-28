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
//!
//! # Verification is fail-closed (PV-0 remediation, 2026-08-20)
//!
//! [`verify_shielded_send`] **never returns `Ok`**. Without
//! `--features real-zk` there is no proof system compiled in, so it rejects
//! with [`ShieldedSendVerifyError::ZkStubAccepted`]; with `real-zk` it still
//! rejects, because flux-zk-stark's verifier is `async`. The one function
//! that can return a [`VerifyOk`] is [`verify::verify_shielded_send_with_stark`]
//! (`real-zk` only), after flux-zk-stark has genuinely verified a proof bound
//! to the tx. See the `verify` module docs for the enforcement mechanism and
//! for what that path still does NOT prove.
//!
//! Practical consequence for integrators: this crate cannot yet be wired into
//! sigil-state's settlement chokepoint. That is deliberate — it fails loudly
//! instead of silently accepting unverified value transfers.

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
pub use verify::{verify_shielded_send, ShieldedSendVerifyError, VerifyOk};

/// The only VerifyOk-producing entry point in the crate — see `verify`.
#[cfg(feature = "real-zk")]
pub use verify::verify_shielded_send_with_stark;

#[cfg(feature = "real-zk")]
pub use stark_proofs::{
    prove_shielded_send_stark, verify_shielded_send_stark, StarkProof,
};
