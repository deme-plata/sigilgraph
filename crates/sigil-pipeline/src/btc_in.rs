//! Leg 1 — **public Bitcoin**. A deposit is admitted only against a self-verifying SPV
//! proof: real proof-of-work on every header, `prev_block` linkage, confirmation depth,
//! merkle inclusion of the deposit tx, and the mainnet difficulty floor. No signer, no
//! committee, no RPC trust. All the cryptography lives in [`sigil_bridge`]; this module
//! turns a verified proof into the one fact the private leg needs: *sats → shield key*.

use sigil_bridge::proof::{dsha256, target_from_nbits};
use sigil_bridge::proof::DEPOSIT_MAGIC;
use sigil_bridge::{process_deposit, BridgeAsset, BridgeLedger, SpvProof};

use crate::PipelineError;

/// Bitcoin mainnet `powLimit` as compact bits (block 0's `nBits`). Any header whose own
/// target is EASIER than this is not a mainnet header, whatever its nonce says.
pub const BITCOIN_MAINNET_POW_LIMIT_NBITS: u32 = 0x1d00_ffff;

/// The Bitcoin genesis block header, 80 bytes, exactly as serialised on the wire.
pub const BITCOIN_GENESIS_HEADER_HEX: &str =
    "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
/// Its hash in the byte order explorers display.
pub const BITCOIN_GENESIS_HASH_DISPLAY: &str =
    "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

/// Mainnet difficulty floor as a 256-bit target.
pub fn mainnet_pow_limit() -> [u8; 32] {
    target_from_nbits(BITCOIN_MAINNET_POW_LIMIT_NBITS)
}

/// Bitcoin hashes are computed little-endian and displayed reversed. Everything in the
/// attestation is shown the way a block explorer would, so a human can look it up.
pub fn display_hash(internal: [u8; 32]) -> String {
    let mut r = internal;
    r.reverse();
    hex::encode(r)
}

pub fn decode_header_hex(s: &str) -> Result<[u8; 80], PipelineError> {
    let v = hex::decode(s.trim()).map_err(|e| PipelineError::Eth(format!("header hex: {e}")))?;
    let arr: [u8; 80] = v
        .try_into()
        .map_err(|_| PipelineError::Eth("a Bitcoin header is exactly 80 bytes".into()))?;
    Ok(arr)
}

/// The admission policy for the Bitcoin leg.
#[derive(Debug, Clone)]
pub struct BtcPolicy {
    /// Headers that must bury the deposit block (the block itself counts as one).
    pub min_confirmations: u32,
    /// `Some(powLimit)` rejects regtest-easy headers. `None` is for tests that mine
    /// their own chain and for nothing else.
    pub pow_limit: Option<[u8; 32]>,
}

impl BtcPolicy {
    pub fn mainnet() -> Self {
        Self { min_confirmations: 6, pow_limit: Some(mainnet_pow_limit()) }
    }
    /// Six confirmations, no floor — the synthetic-chain policy the demo and tests use.
    pub fn synthetic() -> Self {
        Self { min_confirmations: 6, pow_limit: None }
    }
}

/// A deposit the Bitcoin leg has admitted. Every field is bound to proven bytes: `sats`
/// and `shield_pk` come from the memo INSIDE the tx that hashes to `txid`, which the
/// merkle branch proves sits in the block that `confirmations` headers of PoW bury.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBtcDeposit {
    pub txid: [u8; 32],
    pub block_hash: [u8; 32],
    pub confirmations: u32,
    pub sats: u64,
    /// The SIGIL shield public key (wire form) the depositor named. This is the ONLY
    /// SIGIL-side identity Bitcoin ever learns, and it is unlinkable to any ETH address.
    pub shield_pk: [u8; 32],
    /// The bridge peg root after this lock+mint — what a block header would commit to.
    pub supply_root: [u8; 32],
}

/// Build the bytes a deposit tx must carry (in a real Bitcoin tx: an OP_RETURN output).
/// `parse_deposit_intent` scans for the magic anywhere in the tx bytes, so a real
/// serialised transaction with this payload in an OP_RETURN parses identically.
pub fn deposit_memo(sats: u64, shield_pk: [u8; 32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(6 + 16 + 32);
    v.extend_from_slice(DEPOSIT_MAGIC);
    v.extend_from_slice(&(sats as u128).to_le_bytes());
    v.extend_from_slice(&shield_pk);
    v
}

/// Verify the proof under `policy` and, only then, lock + mint under the peg chokepoint.
pub fn admit_deposit(
    ledger: &mut BridgeLedger,
    proof: &SpvProof,
    policy: &BtcPolicy,
) -> Result<VerifiedBtcDeposit, PipelineError> {
    // The policy may demand MORE confirmations than the asset default; the stricter wins.
    proof.verify(policy.min_confirmations)?;
    let receipt = process_deposit(ledger, BridgeAsset::Btc, proof, policy.pow_limit.as_ref())?;
    let intent = proof.deposit_intent()?;
    let sats = u64::try_from(intent.amount)
        .map_err(|_| PipelineError::DepositTooLarge { sats: u64::MAX })?;
    Ok(VerifiedBtcDeposit {
        txid: proof.tx_hash,
        block_hash: proof.block_hash().expect("verify() rejects an empty chain"),
        confirmations: proof.headers.len() as u32,
        sats,
        shield_pk: intent.recipient,
        supply_root: receipt.supply_root,
    })
}

/// A minimal synthetic Bitcoin chain for tests and the demo: single-tx blocks mined at the
/// regtest floor. Real PoW (the nonce is ground), real linkage, real merkle (root == leaf).
pub mod synthetic {
    use super::*;
    use sigil_bridge::proof::header_meets_target;

    pub const EASY_NBITS: u32 = 0x207f_ffff;

    pub fn mine(prev: [u8; 32], merkle_root: [u8; 32]) -> [u8; 80] {
        let mut h = [0u8; 80];
        h[0] = 1;
        h[4..36].copy_from_slice(&prev);
        h[36..68].copy_from_slice(&merkle_root);
        h[72..76].copy_from_slice(&EASY_NBITS.to_le_bytes());
        let mut nonce = 0u32;
        loop {
            h[76..80].copy_from_slice(&nonce.to_le_bytes());
            if header_meets_target(&h) {
                return h;
            }
            nonce += 1;
        }
    }

    /// A deposit tx buried under `confirmations` headers.
    pub fn deposit_proof(sats: u64, shield_pk: [u8; 32], confirmations: u32) -> SpvProof {
        let mut tx = b"btc-tx:".to_vec();
        tx.extend_from_slice(&deposit_memo(sats, shield_pk));
        let leaf = dsha256(&tx);
        let mut headers = vec![mine([0u8; 32], leaf)];
        for _ in 1..confirmations {
            let prev = dsha256(headers.last().unwrap());
            headers.push(mine(prev, [7u8; 32]));
        }
        SpvProof { tx_bytes: tx, tx_hash: leaf, branch: vec![], tx_index: 0, headers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_bridge::proof::header_meets_target;

    #[test]
    fn bitcoin_genesis_header_is_real_pow_under_the_mainnet_floor() {
        let h = decode_header_hex(BITCOIN_GENESIS_HEADER_HEX).unwrap();
        assert_eq!(display_hash(dsha256(&h)), BITCOIN_GENESIS_HASH_DISPLAY);
        assert!(header_meets_target(&h), "block 0 must satisfy its own nBits");
        let proof = SpvProof { tx_bytes: vec![], tx_hash: [0; 32], branch: vec![], tx_index: 0, headers: vec![h] };
        proof.verify_difficulty_floor(&mainnet_pow_limit()).expect("genesis is AT the floor, not under it");
    }

    #[test]
    fn a_regtest_header_is_refused_by_the_mainnet_floor() {
        let proof = synthetic::deposit_proof(1_000, [9u8; 32], 6);
        assert!(admit_deposit(&mut BridgeLedger::new(), &proof, &BtcPolicy::synthetic()).is_ok());
        let err = admit_deposit(&mut BridgeLedger::new(), &proof, &BtcPolicy::mainnet()).unwrap_err();
        assert!(matches!(err, PipelineError::Btc(_)), "got {err}");
    }

    #[test]
    fn deposit_binds_sats_and_shield_key_to_the_proven_tx() {
        let pk = [0xAB; 32];
        let proof = synthetic::deposit_proof(250_000, pk, 6);
        let mut ledger = BridgeLedger::new();
        let d = admit_deposit(&mut ledger, &proof, &BtcPolicy::synthetic()).unwrap();
        assert_eq!(d.sats, 250_000);
        assert_eq!(d.shield_pk, pk);
        assert_eq!(d.confirmations, 6);
        assert_eq!(d.txid, proof.tx_hash);
        // replay: the same proven deposit mints exactly once
        assert!(matches!(
            admit_deposit(&mut ledger, &proof, &BtcPolicy::synthetic()),
            Err(PipelineError::Btc(sigil_bridge::BridgeError::ReplayedDeposit))
        ));
    }

    #[test]
    fn five_confirmations_are_not_six() {
        let proof = synthetic::deposit_proof(1, [1u8; 32], 5);
        assert!(matches!(
            admit_deposit(&mut BridgeLedger::new(), &proof, &BtcPolicy::synthetic()),
            Err(PipelineError::Spv(sigil_bridge::ProofError::InsufficientConfirmations { got: 5, need: 6 }))
        ));
    }
}
