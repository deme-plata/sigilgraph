//! ln.rs — the Lightning rail. Instant BTC↔wBTC with NO on-chain confirmation
//! wait — the fast complement to the on-chain SPV rail in [`crate::proof`].
//!
//! Two proofs are required to mint, and BOTH are now cryptographic (audit C9):
//!   1. The **signed BOLT11 invoice** — parsing it with [`Bolt11Invoice::from_str`]
//!      verifies the payee node's secp256k1 signature over the invoice, so the
//!      `amount` and `payment_hash` are bound to what the issuing node actually
//!      signed. (Previously `amount_msat` was a free caller field — a depositor
//!      could settle a 1-sat invoice and declare 50k.)
//!   2. The **payment preimage** — a Lightning invoice's
//!      `payment_hash = SHA256(preimage)`, revealed only on settlement, so
//!      holding it proves the invoice was paid.
//!
//! With an `expected_payee` pubkey, the bridge additionally requires the invoice
//! to have been issued by ITS OWN LN node — so a valid invoice from some
//! unrelated node can't be used to mint.

use std::str::FromStr;

use lightning_invoice::Bolt11Invoice;
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
    #[error("preimage does not match the invoice payment_hash — invoice not proven paid")]
    PreimageMismatch,
    #[error("invoice has no amount (amountless invoices cannot mint)")]
    ZeroAmount,
    #[error("BOLT11 parse/signature verification failed: {0}")]
    BadInvoice(String),
    #[error("invoice was not issued by the bridge's LN node (payee pubkey mismatch)")]
    WrongPayee,
}

/// A settled-invoice proof: the signed BOLT11 + the preimage proving it was paid.
#[derive(Debug, Clone)]
pub struct LnProof {
    /// The signed BOLT11 invoice string (`lnbc…`). Its signature is verified.
    pub bolt11: String,
    /// The payment preimage; `SHA256(preimage)` must equal the invoice hash.
    pub preimage: [u8; 32],
}

/// The verified, invoice-bound result of an [`LnProof`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLn {
    /// Satoshis to mint, taken from the SIGNED invoice (wBTC is sat-denominated).
    pub amount_sats: u128,
    /// The invoice's payment hash (the bridge spent-set key).
    pub payment_hash: [u8; 32],
}

impl LnProof {
    /// Parse + verify the BOLT11 (checks the payee's secp256k1 signature), bind
    /// the amount + payment_hash to the signed invoice, and confirm the preimage
    /// proves payment. If `expected_payee` is `Some`, the invoice's payee public
    /// key (33-byte compressed, or 32-byte x-only is NOT accepted here) must match
    /// — so only invoices issued by the bridge's own LN node can mint.
    pub fn verify(&self, expected_payee: Option<&[u8]>) -> Result<VerifiedLn, LnError> {
        let inv = Bolt11Invoice::from_str(&self.bolt11)
            .map_err(|e| LnError::BadInvoice(format!("{e:?}")))?;

        let msat = inv.amount_milli_satoshis().ok_or(LnError::ZeroAmount)?;
        if msat == 0 {
            return Err(LnError::ZeroAmount);
        }

        if let Some(exp) = expected_payee {
            // recover_payee_pub_key() returns the secp256k1 PublicKey that signed
            // the invoice; serialize() is the 33-byte compressed form.
            let payee = inv.recover_payee_pub_key();
            if payee.serialize().as_slice() != exp {
                return Err(LnError::WrongPayee);
            }
        }

        let mut ph = [0u8; 32];
        ph.copy_from_slice(inv.payment_hash().as_ref());
        if sha256(&self.preimage) != ph {
            return Err(LnError::PreimageMismatch);
        }
        Ok(VerifiedLn { amount_sats: (msat / 1000) as u128, payment_hash: ph })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin_hashes::{sha256 as bh_sha256, Hash};
    use lightning_invoice::{Currency, InvoiceBuilder};
    use secp256k1::{Secp256k1, SecretKey};

    // Build a real, SIGNED BOLT11 invoice for `amount_msat` whose payment_hash is
    // SHA256(preimage), signed by `sk`. Returns (bolt11_string, payee_compressed_pubkey).
    fn signed_invoice(amount_msat: u64, preimage: [u8; 32], sk_bytes: [u8; 32]) -> (String, Vec<u8>) {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&sk_bytes).expect("sk");
        let payee = sk.public_key(&secp);
        let ph = bh_sha256::Hash::hash(&preimage);
        let payment_secret = lightning_invoice::PaymentSecret([0x42u8; 32]);
        let inv = InvoiceBuilder::new(Currency::Bitcoin)
            .description("sigil-bridge test".into())
            .payment_hash(ph)
            .payment_secret(payment_secret)
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(amount_msat)
            .build_signed(|h| secp.sign_ecdsa_recoverable(h, &sk))
            .expect("build invoice");
        (inv.to_string(), payee.serialize().to_vec())
    }

    #[test]
    fn signed_invoice_binds_amount_and_verifies() {
        let preimage = [7u8; 32];
        let (bolt11, payee) = signed_invoice(250_000_000, preimage, [0x11; 32]); // 250k sat
        let p = LnProof { bolt11, preimage };
        let v = p.verify(Some(&payee)).expect("verify");
        assert_eq!(v.amount_sats, 250_000, "amount is bound to the SIGNED invoice");
        assert_eq!(v.payment_hash, sha256(&preimage));
    }

    #[test]
    fn wrong_preimage_rejected() {
        let (bolt11, payee) = signed_invoice(1000, [7u8; 32], [0x11; 32]);
        let p = LnProof { bolt11, preimage: [9u8; 32] }; // wrong preimage
        assert_eq!(p.verify(Some(&payee)), Err(LnError::PreimageMismatch));
    }

    #[test]
    fn wrong_payee_rejected() {
        // invoice signed by key A; bridge expects key B → rejected.
        let preimage = [7u8; 32];
        let (bolt11, _payee_a) = signed_invoice(1000, preimage, [0x11; 32]);
        let secp = Secp256k1::new();
        let other = SecretKey::from_slice(&[0x22; 32]).unwrap().public_key(&secp).serialize().to_vec();
        let p = LnProof { bolt11, preimage };
        assert_eq!(p.verify(Some(&other)), Err(LnError::WrongPayee));
    }

    #[test]
    fn garbage_invoice_rejected() {
        let p = LnProof { bolt11: "not-a-real-invoice".into(), preimage: [0u8; 32] };
        assert!(matches!(p.verify(None), Err(LnError::BadInvoice(_))));
    }
}
