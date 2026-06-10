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
pub mod withdraw;

pub use aggregate::{fold_epoch_deposits, DepositRecord};

pub use asset::BridgeAsset;
pub use ledger::{AssetPeg, BridgeLedger, PegError};
pub use ln::{LnError, LnProof};
pub use proof::{dsha256, parse_deposit_intent, verify_merkle_inclusion, DepositIntent, ProofError, SpvProof};
pub use withdraw::WithdrawalRequest;

/// Why a deposit/withdrawal was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BridgeError {
    #[error("proof verification failed: {0}")]
    Proof(#[from] ProofError),
    #[error("peg violation: {0}")]
    Peg(#[from] PegError),
    #[error("lightning proof failed: {0}")]
    Ln(#[from] LnError),
    #[error("unsupported asset for SPV verification: {0:?} (only BTC has a real SPV PoW verifier — others must not be validated against Bitcoin headers)")]
    UnsupportedAsset(BridgeAsset),
    #[error("deposit proof already minted against (replay)")]
    ReplayedDeposit,
    #[error("withdrawal not authorized by the owner (bad signature)")]
    UnauthorizedWithdrawal,
    #[error("withdrawal already processed (replay)")]
    ReplayedWithdrawal,
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
/// Audit C9/H7 hardening (vs the old `(ledger, asset, amount, recipient, proof)`):
/// - **amount + recipient are now BOUND to the proven tx** — read from the
///   deposit memo inside `proof.tx_bytes` (which hashes to the proven, buried
///   `tx_hash`), NOT taken as independent caller args. Closes the free-mint
///   hole (prove a 1-sat tx → mint 21M to yourself).
/// - **replay guard** — each proven `tx_hash` mints exactly once (spent-set).
/// - **per-asset gate** — only BTC has a real SPV PoW verifier; ETH/ZEC/IRON are
///   rejected rather than "validated" against Bitcoin headers.
/// - **difficulty floor** — pass `min_pow_target` (the chain's `powLimit`) to
///   reject regtest-easy forged headers; `None` skips the floor (tests only).
pub fn process_deposit(
    ledger: &mut BridgeLedger,
    asset: BridgeAsset,
    proof: &SpvProof,
    min_pow_target: Option<&[u8; 32]>,
) -> Result<MintReceipt, BridgeError> {
    // 0. per-asset gate (H7): the SPV verifier is Bitcoin-PoW only.
    if asset != BridgeAsset::Btc {
        return Err(BridgeError::UnsupportedAsset(asset));
    }
    // 1. cryptographic verification FIRST — no mint without a valid proof.
    proof.verify(asset.min_confirmations())?;
    if let Some(floor) = min_pow_target {
        proof.verify_difficulty_floor(floor)?;
    }
    // 2. amount + recipient are bound to the proven tx, not caller-claimed.
    let intent = proof.deposit_intent()?;
    // 3. replay guard: one mint per proven deposit.
    if ledger.is_spent(&proof.tx_hash) {
        return Err(BridgeError::ReplayedDeposit);
    }
    // 4. lock the proven collateral, then mint under the peg chokepoint.
    ledger.lock(asset, intent.amount)?;
    ledger.mint(asset, intent.amount)?;
    ledger.mark_spent(proof.tx_hash);
    Ok(MintReceipt {
        asset,
        amount: intent.amount,
        recipient: hex::encode(intent.recipient),
        supply_root: ledger.root(),
    })
}

/// Withdraw: verify the owner's signature, enforce idempotency, then burn
/// wrapped + release the collateral (source-chain payout to `req.dest` happens
/// off this call). Returns the post-burn supply root.
///
/// Audit C9: was `(ledger, asset, amount)` with NO authorization, owner, payout
/// destination, or replay guard — anyone could drain locked collateral. Now a
/// [`WithdrawalRequest`] must carry the owner's ed25519 signature over the exact
/// `(asset, amount, owner, dest, nonce)`, and its deterministic id is recorded
/// so the same authorization can't be processed twice.
pub fn process_withdrawal(
    ledger: &mut BridgeLedger,
    req: &WithdrawalRequest,
) -> Result<[u8; 32], BridgeError> {
    if !req.verify() {
        return Err(BridgeError::UnauthorizedWithdrawal);
    }
    let id = req.id();
    if ledger.is_withdrawal_processed(&id) {
        return Err(BridgeError::ReplayedWithdrawal);
    }
    ledger.burn(req.asset, req.amount)?;
    ledger.unlock(req.asset, req.amount)?;
    ledger.mark_withdrawal(id);
    Ok(ledger.root())
}

/// LIGHTNING deposit: verify the settled-invoice preimage, then lock + mint wBTC
/// (instant — no on-chain confirmations). The fast rail; same peg chokepoint.
/// C9: the `payment_hash` is now recorded in the spent-set so one settled
/// invoice can't be replayed for repeated mints. (BOLT11-signature binding of
/// amount + payee is the remaining documented hardening lane.)
pub fn process_ln_deposit(
    ledger: &mut BridgeLedger,
    recipient: impl Into<String>,
    ln: &LnProof,
    expected_payee: Option<&[u8]>,
) -> Result<MintReceipt, BridgeError> {
    // Verifies the BOLT11 secp256k1 signature, binds amount + payment_hash to the
    // SIGNED invoice, and checks the preimage proves payment. `expected_payee`
    // (the bridge's own LN node pubkey) restricts mints to invoices it issued.
    let v = ln.verify(expected_payee)?;
    if ledger.is_spent(&v.payment_hash) {
        return Err(BridgeError::ReplayedDeposit);
    }
    ledger.lock(BridgeAsset::Btc, v.amount_sats)?;
    ledger.mint(BridgeAsset::Btc, v.amount_sats)?;
    ledger.mark_spent(v.payment_hash);
    Ok(MintReceipt { asset: BridgeAsset::Btc, amount: v.amount_sats, recipient: recipient.into(), supply_root: ledger.root() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proof::{dsha256, header_meets_target, DEPOSIT_MAGIC};
    use sha2::{Digest, Sha256};
    use sigil_oauth::Keypair;

    const EASY_NBITS: u32 = 0x207f_ffff;

    /// Build a source-chain tx embedding the SIGIL deposit memo (magic ‖
    /// amount-LE ‖ recipient) so `deposit_intent()` binds amount + recipient.
    fn deposit_tx(amount: u128, recipient: [u8; 32]) -> Vec<u8> {
        let mut v = b"src-chain-tx-prefix".to_vec();
        v.extend_from_slice(DEPOSIT_MAGIC);
        v.extend_from_slice(&amount.to_le_bytes());
        v.extend_from_slice(&recipient);
        v
    }
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
        use bitcoin_hashes::{sha256 as bh_sha256, Hash};
        use lightning_invoice::{Currency, InvoiceBuilder};
        use secp256k1::{Secp256k1, SecretKey};
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[0x11; 32]).unwrap();
        let payee = sk.public_key(&secp).serialize().to_vec();
        let preimage = [7u8; 32];
        let inv = InvoiceBuilder::new(Currency::Bitcoin)
            .description("ln deposit".into())
            .payment_hash(bh_sha256::Hash::hash(&preimage))
            .payment_secret(lightning_invoice::PaymentSecret([0x42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(250_000_000) // 250k sat
            .build_signed(|h| secp.sign_ecdsa_recoverable(h, &sk))
            .unwrap();
        let ln = LnProof { bolt11: inv.to_string(), preimage };
        let r = process_ln_deposit(&mut ledger_new(), "qnk_ln_user", &ln, Some(&payee)).unwrap();
        assert_eq!(r.amount, 250_000, "amount bound to the SIGNED invoice");
        let mut l = BridgeLedger::new();
        process_ln_deposit(&mut l, "qnk_ln_user", &ln, Some(&payee)).unwrap();
        assert_eq!(l.peg(BridgeAsset::Btc), AssetPeg { locked: 250_000, minted: 250_000 });
        assert!(l.peg_ok());
    }

    fn ledger_new() -> BridgeLedger { BridgeLedger::new() }

    #[test]
    fn deposit_mints_amount_and_recipient_bound_to_the_proof() {
        let mut ledger = BridgeLedger::new();
        let recipient = [0xAB; 32];
        let proof = valid_proof_for(&deposit_tx(100_000, recipient), 6);
        let receipt = process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None).unwrap();
        // amount + recipient came FROM the proven tx, not a caller arg.
        assert_eq!(receipt.amount, 100_000);
        assert_eq!(receipt.recipient, hex::encode(recipient));
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 100_000, minted: 100_000 });
        assert!(ledger.peg_ok());
        // C9: replaying the same proof is rejected — no double-mint.
        assert!(matches!(
            process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None),
            Err(BridgeError::ReplayedDeposit)
        ));
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 100_000, minted: 100_000 });
    }

    #[test]
    fn deposit_with_too_few_confirmations_is_rejected_no_mint() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(&deposit_tx(50_000, [1u8; 32]), 2); // < 6 for BTC
        let err = process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None).unwrap_err();
        assert!(matches!(err, BridgeError::Proof(ProofError::InsufficientConfirmations { .. })));
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg::default());
    }

    #[test]
    fn deposit_without_memo_is_rejected() {
        // A proof for a tx with NO SIGIL deposit memo can't bind an amount → no mint.
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(b"btc-tx-with-no-sigil-memo", 6);
        assert!(matches!(
            process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None),
            Err(BridgeError::Proof(ProofError::NoDepositMemo))
        ));
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg::default());
    }

    #[test]
    fn non_btc_spv_deposit_is_rejected() {
        // ETH/ZEC/IRON must NOT be "validated" against Bitcoin PoW headers (H7).
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(&deposit_tx(1_000, [3u8; 32]), 10);
        assert!(matches!(
            process_deposit(&mut ledger, BridgeAsset::Zec, &proof, None),
            Err(BridgeError::UnsupportedAsset(BridgeAsset::Zec))
        ));
    }

    #[test]
    fn difficulty_floor_rejects_easy_headers() {
        // With a strict floor (impossibly-hard target), the regtest-easy headers
        // that pass bare verify() are rejected (H7: self-declared difficulty).
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(&deposit_tx(1_000, [5u8; 32]), 6);
        let strict = [0u8; 32];
        assert!(matches!(
            process_deposit(&mut ledger, BridgeAsset::Btc, &proof, Some(&strict)),
            Err(BridgeError::Proof(ProofError::DifficultyTooLow(_)))
        ));
    }

    #[test]
    fn deposit_then_signed_withdraw_roundtrip() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(&deposit_tx(7_000, [2u8; 32]), 6);
        process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None).unwrap();

        let kp = Keypair::from_seed(&[8u8; 32]);
        let mut req = WithdrawalRequest {
            asset: BridgeAsset::Btc, amount: 7_000, owner: kp.pubkey(),
            dest: "bc1qpayout".into(), nonce: 1, sig: [0u8; 64],
        };
        req.sig = kp.sign(&req.signing_bytes());
        process_withdrawal(&mut ledger, &req).unwrap();
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 0, minted: 0 });
        // C9: the same signed request can't be processed twice.
        assert!(matches!(
            process_withdrawal(&mut ledger, &req),
            Err(BridgeError::ReplayedWithdrawal)
        ));
    }

    #[test]
    fn unauthorized_withdrawal_is_rejected_collateral_untouched() {
        let mut ledger = BridgeLedger::new();
        let proof = valid_proof_for(&deposit_tx(5_000, [4u8; 32]), 6);
        process_deposit(&mut ledger, BridgeAsset::Btc, &proof, None).unwrap();
        // zero-sig request (no owner authorization) → rejected, peg untouched.
        let req = WithdrawalRequest {
            asset: BridgeAsset::Btc, amount: 5_000, owner: [9u8; 32],
            dest: "x".into(), nonce: 1, sig: [0u8; 64],
        };
        assert!(matches!(
            process_withdrawal(&mut ledger, &req),
            Err(BridgeError::UnauthorizedWithdrawal)
        ));
        assert_eq!(ledger.peg(BridgeAsset::Btc), AssetPeg { locked: 5_000, minted: 5_000 });
    }
}
