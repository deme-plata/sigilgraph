//! ln.rs — the Lightning rail (LNbits-backed). Instant BTC↔wBTC with NO on-chain
//! confirmation wait — the fast complement to the SPV rail in [`crate::proof`].
//!
//! The proof here is the payment **preimage**. A Lightning invoice's
//! `payment_hash = SHA256(preimage)`, and the preimage is revealed ONLY when the
//! invoice settles — so holding it cryptographically proves the invoice was paid.
//! That's the Lightning analog of the on-chain SPV proof: mint against a proof,
//! not a trusted "we got paid" attestation.
//!
//! Backend: **LNbits**. The relayer creates an invoice via the LNbits API
//! (`POST /api/v1/payments`, `X-Api-Key`), hands the BOLT11 to the depositor,
//! and on settlement reads back the preimage (`GET /api/v1/payments/<hash>` →
//! `preimage`). That preimage + payment_hash + amount become an [`LnProof`].
//!
//! Hardening lane (documented): parse + verify the signed BOLT11 so the amount
//! and destination are bound by the payee node's signature rather than reported
//! by our own LNbits. Today the preimage proves THIS invoice (by hash) was paid.

use sha2::{Digest, Sha256};

fn sha256(data: &[u8]) -> [u8; 32] {
    let d = Sha256::digest(data);
    let mut o = [0u8; 32];
    o.copy_from_slice(&d);
    o
}

/// Why a Lightning proof was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LnError {
    #[error("preimage does not match payment_hash — invoice not proven paid")]
    PreimageMismatch,
    #[error("zero-amount invoice")]
    ZeroAmount,
}

/// A settled-invoice proof: the preimage whose SHA256 is the invoice's hash.
#[derive(Debug, Clone)]
pub struct LnProof {
    pub payment_hash: [u8; 32],
    pub preimage: [u8; 32],
    pub amount_msat: u64,
}

impl LnProof {
    /// `SHA256(preimage) == payment_hash` ⇒ the invoice was paid.
    pub fn verify(&self) -> Result<(), LnError> {
        if self.amount_msat == 0 {
            return Err(LnError::ZeroAmount);
        }
        if sha256(&self.preimage) != self.payment_hash {
            return Err(LnError::PreimageMismatch);
        }
        Ok(())
    }

    /// Satoshis (wBTC is sat-denominated; LN amounts are msat).
    pub fn amount_sats(&self) -> u128 {
        (self.amount_msat / 1000) as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paid_invoice_verifies_and_converts() {
        let preimage = [42u8; 32];
        let p = LnProof { payment_hash: sha256(&preimage), preimage, amount_msat: 50_000_000 };
        assert!(p.verify().is_ok());
        assert_eq!(p.amount_sats(), 50_000);
    }

    #[test]
    fn wrong_preimage_rejected() {
        let p = LnProof { payment_hash: [1u8; 32], preimage: [2u8; 32], amount_msat: 1000 };
        assert_eq!(p.verify(), Err(LnError::PreimageMismatch));
    }

    #[test]
    fn zero_amount_rejected() {
        let preimage = [9u8; 32];
        let p = LnProof { payment_hash: sha256(&preimage), preimage, amount_msat: 0 };
        assert_eq!(p.verify(), Err(LnError::ZeroAmount));
    }
}
