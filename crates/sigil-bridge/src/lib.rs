//! # sigil-bridge — the proof-carrying, supply-committed cross-chain bridge.
//!
//! **One better than Quillon.** Quillon mints a wrapped token when a 7-of-11
//! rotating committee *signs an attestation* ("we saw your deposit") over a
//! custodial hot wallet — with a single-node-mint fallback if the committee is
//! thin. You trust the signers. SIGIL mints only against a [`SpvProof`] the node
//! **verifies cryptographically** (tx → merkle-root → block-header → block-hash,
//! plus a finality depth), and commits the **peg invariant** (`minted ≤ locked`)
//! into a [`BridgeLedger::root`] every block. Proof, not trust; committed, not
//! opaque.
//!
//! The deposit flow:
//! ```text
//!   1. user sends BTC/ETH/ZEC/IRON on the source chain
//!   2. a relayer (no special trust) fetches the SPV proof from a source-chain
//!      node (e.g. Delta's Bitcoin Knots RPC for BTC) and submits it
//!   3. process_deposit() VERIFIES the proof + finality, locks the collateral,
//!      mints the wrapped token under the peg chokepoint
//!   4. the new bridge-supply root is committed in the next SIGIL block
//! ```
//!
//! Withdrawal is the mirror: burn wrapped → unlock → pay out on the source chain.

pub mod aggregate;
pub mod asset;
pub mod ledger;
pub mod ln;
pub mod proof;

pub use aggregate::{fold_epoch_deposits, DepositRecord};

pub use asset::BridgeAsset;
pub use ledger::{AssetPeg, BridgeLedger, PegError};
pub use ln::{LnError, LnProof};
pub use proof::{dsha256, verify_merkle_inclusion, ProofError, SpvProof};

/// Why a deposit was not minted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BridgeError {
    #[error("proof verification failed: {0}")]
    Proof(#[from] ProofError),
    #[error("peg violation: {0}")]
    Peg(#[from] PegError),
    #[error("lightning proof failed: {0}")]
    Ln(#[from] LnError),
}

/// The receipt of a successful, proof-backed mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintReceipt {
    pub asset: BridgeAsset,
    pub amount: u128,
    pub recipient: String,
    /// The bridge-supply root AFTER this mint (goes into the block header).
    pub supply_root: [u8; 32],
}

/// Verify a source-chain deposit proof and, only if it checks out, lock the
/// collateral + mint the wrapped token under the peg chokepoint. The single
/// entry point a node calls — verification is mandatory and first.
///
/// `amount`/`recipient` are the deposit's value + SIGIL destination. Binding
/// them to fields parsed out of `proof.tx_bytes` (so they can't be claimed
/// independently of the proven tx) is the documented hardening lane; today the
/// proof binds the *inclusion* of a specific tx and the mint is gated on it.
pub fn process_deposit(
    ledger: &mut BridgeLedger,
    asset: BridgeAsset,
    amount: u128,
    recipient: impl Into<String>,
    proof: &SpvProof,
) -> Result<MintReceipt, BridgeError> {
    // 1. cryptographic verification FIRST — no mint without a valid proof.
    proof.verify(asset.min_confirmations())?;
    // 2. lock the proven collateral, then mint under the peg chokepoint.
    ledger.lock(asset, amount)?;
    ledger.mint(asset, amount)?;
    Ok(MintReceipt { asset, amount, recipient: recipient.into(), supply_root: ledger.root() })
}

/// Withdraw: burn wrapped + release the collateral (source-chain payout happens
/// off this call). Returns the post-burn supply root.
pub fn process_withdrawal(ledger: &mut BridgeLedger, asset: BridgeAsset, amount: u128) -> Result<[u8; 32], BridgeError> {
    ledger.burn(asset, amount)?;
    ledger.unlock(asset, amount)?;
    Ok(ledger.root())
}

/// LIGHTNING deposit: verify the settled-invoice preimage, then lock + mint wBTC
/// (instant — no on-chain confirmations). The fast rail; same peg chokepoint.
pub fn process_ln_deposit(
    ledger: &mut BridgeLedger,
    recipient: impl Into<String>,
    ln: &LnProof,
) -> Result<MintReceipt, BridgeError> {
    ln.verify()?; // SHA256(preimage) == payment_hash ⇒ paid
    let amount = ln.amount_sats();
    ledger.lock(BridgeAsset::Btc, amount)?;
    ledger.mint(BridgeAsset::Btc, amount)?;
    Ok(MintReceipt { asset: BridgeAsset::Btc, amount, recipient: recipient.into(), supply_root: ledger.root() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proof::{dsha256, header_meets_target};
    use sha2::{Digest, Sha256};

    const EASY_NBITS: u32 = 0x207f_ffff;
    fn mine(prev: [u8; 32], merkle_root: [u8; 32]) -> [u8; 80] {
        let mut h = [0u8; 80];
        h[0] = 1;
        h[4..36].copy_from_slice(&prev);
        h[36..68].copy_from_slice(&merkle_root);
        h[72..76].copy_from_slice(&EASY_NBITS.to_le_bytes());
        let mut n = 0u32;
        loop {
            h[76..80].copy_from_slice(&n.to_le_bytes());
            if header_meets_target(&h) {
                return h;
            }
            n += 1;
        }
    }

    /// A single-tx deposit block + (`confirmations`-1) PoW successors on top.
    fn valid_proof_for(tx: &[u8], confirmations: u32) -> SpvProof {
        let leaf = dsha256(tx);
        let mut headers = vec![mine([0u8; 32], leaf)]; // single-tx block: root == leaf
        for _ in 1..confirmations {
            let prev = dsha256(headers.last().unwrap());
            headers.push(mine(prev, [7u8; 32]));
        }
        SpvProof { tx_bytes: tx.to_vec(), tx_hash: leaf, branch: vec![], tx_index: 0, headers }
    }

    #[test]
    fn lightning_deposit_mints_wbtc_instantly() {
        let mut ledger = BridgeLedger::new();
        let preimage = [7u8; 32];
        let payment_hash = {
            let d = Sha256::digest(preimage);
            let mut o = [0u8; 32];
            o.copy_from_slice(&d);
            o
        };
        let ln = LnProof { payment_hash, preimage, amount_msat: 250_000_000 }; // 250k sat
        let r = process_ln_deposit(&mut ledger, "qnk_ln_user", &ln).unwrap();
        assert_eq!(r.amount, 250_000);
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 250_000, minted: 250_000 });
        assert!(ledger.peg_ok());
    }

    #[test]
    fn deposit_mints_only_with_a_valid_proof() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(b"btc-deposit-tx-100k-sats", 6);
        let receipt = process_deposit(&mut ledger, BridgeAsset::Btc, 100_000, "qnk_alice", &proof).unwrap();
        assert_eq!(receipt.amount, 100_000);
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 100_000, minted: 100_000 });
        assert!(ledger.peg_ok());
    }

    #[test]
    fn deposit_with_too_few_confirmations_is_rejected_no_mint() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(b"btc-deposit", 2); // < 6 for BTC
        let err = process_deposit(&mut ledger, BridgeAsset::Btc, 50_000, "qnk_bob", &proof).unwrap_err();
        assert!(matches!(err, BridgeError::Proof(ProofError::InsufficientConfirmations { .. })));
        // nothing minted or locked
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg::default());
    }

    #[test]
    fn deposit_then_withdraw_roundtrip() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(b"zec-deposit", 10);
        process_deposit(&mut ledger, BridgeAsset::Zec, 7_000, "qnk_carol", &proof).unwrap();
        process_withdrawal(&mut ledger, BridgeAsset::Zec, 7_000).unwrap();
        assert_eq!(ledger.peg(BridgeAsset::Zec), AssetPeg { locked: 0, minted: 0 });
    }
}
