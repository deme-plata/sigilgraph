//! withdraw.rs — signed, idempotent withdrawal authorization (audit C9).
//!
//! The old `process_withdrawal(ledger, asset, amount)` burned wrapped tokens and
//! released collateral with NO proof the caller owned the wrapped tokens, no
//! payout destination, and no replay guard — anyone could drain the locked
//! collateral and the off-chain payout target was unspecified. A withdrawal now
//! carries an ed25519 signature by the wrapped-token holder over the exact
//! `(asset, amount, owner, dest, nonce)`, and a deterministic id the ledger
//! records so the same authorization can't be processed twice.

use crate::asset::BridgeAsset;
use sigil_oauth::verify_sig;

/// A signed request to withdraw `amount` of `asset`, releasing the source-chain
/// collateral to `dest`. `owner` is the wrapped-token holder's wallet (= their
/// ed25519 public key); `sig` proves they authorized exactly this withdrawal.
#[derive(Debug, Clone)]
pub struct WithdrawalRequest {
    pub asset: BridgeAsset,
    pub amount: u128,
    /// Wrapped-token holder authorizing the burn (= ed25519 pubkey / wallet).
    pub owner: [u8; 32],
    /// Source-chain payout destination for the unlocked collateral.
    pub dest: String,
    /// Per-owner monotonic nonce — distinguishes otherwise-identical requests
    /// and (with the ledger's processed-set) makes the request single-use.
    pub nonce: u64,
    /// ed25519 signature by `owner` over [`Self::signing_bytes`].
    pub sig: [u8; 64],
}

impl WithdrawalRequest {
    /// Canonical, domain-separated bytes the owner signs. Binds every field so
    /// none can be altered after signing.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut m = b"sigil-bridge/withdraw/v1:".to_vec();
        m.push(self.asset.tag());
        m.extend_from_slice(&self.amount.to_le_bytes());
        m.extend_from_slice(&self.owner);
        m.extend_from_slice(self.dest.as_bytes());
        m.extend_from_slice(&self.nonce.to_le_bytes());
        m
    }

    /// Deterministic idempotency id = BLAKE3(signing_bytes). The ledger records
    /// this so a captured request can't burn/unlock (or trigger a payout) twice.
    pub fn id(&self) -> [u8; 32] {
        *blake3::hash(&self.signing_bytes()).as_bytes()
    }

    /// True iff `sig` proves `owner` authorized exactly this withdrawal.
    pub fn verify(&self) -> bool {
        verify_sig(&self.owner, &self.signing_bytes(), &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oauth::Keypair;

    #[test]
    fn signed_request_verifies_and_tamper_fails() {
        let kp = Keypair::from_seed(&[5u8; 32]);
        let owner = kp.pubkey();
        let mut req = WithdrawalRequest {
            asset: BridgeAsset::Btc, amount: 1000, owner, dest: "bc1qexample".into(), nonce: 1,
            sig: [0u8; 64],
        };
        req.sig = kp.sign(&req.signing_bytes());
        assert!(req.verify());
        // tamper the amount after signing → signature no longer authorizes it.
        let mut bad = req.clone();
        bad.amount = 1_000_000;
        assert!(!bad.verify());
    }

    #[test]
    fn wrong_owner_rejected() {
        let signer = Keypair::from_seed(&[5u8; 32]);
        let other = Keypair::from_seed(&[6u8; 32]).pubkey();
        let mut req = WithdrawalRequest {
            asset: BridgeAsset::Btc, amount: 1000, owner: other, dest: "bc1q".into(), nonce: 1,
            sig: [0u8; 64],
        };
        // signed by `signer`, but `owner` field is someone else → verify fails.
        req.sig = signer.sign(&req.signing_bytes());
        assert!(!req.verify());
    }
}
