//! aggregate.rs — fold a whole epoch's verified deposits into ONE constant-size
//! flux-fold attestation. (This is the "what can we use flux-fold for" answer,
//! shipped: instead of relaying N individual deposit proofs, the bridge commits
//! one fixed-size object per epoch covering every deposit.)

use crate::asset::BridgeAsset;
use flux_fold::{fold, Ajtai, FoldedProof, Q};

const BRIDGE_FOLD_SEED: [u8; 32] = *b"sigil-bridge/epoch-fold/v1//////";
const AJTAI_M: usize = 4;
const AJTAI_N: usize = 8;

/// One verified deposit's public facts (what gets committed).
#[derive(Debug, Clone)]
pub struct DepositRecord {
    pub asset: BridgeAsset,
    pub amount: u128,
    pub recipient: String,
    pub source_block_hash: [u8; 32],
}

/// Encode a deposit as a flux-fold witness vector (len n).
fn witness(d: &DepositRecord) -> Vec<u64> {
    let mut h = blake3::Hasher::new();
    h.update(&[d.asset.tag()]);
    h.update(&d.amount.to_le_bytes());
    h.update(d.recipient.as_bytes());
    h.update(&d.source_block_hash);
    let b = *h.finalize().as_bytes();
    let mut w = vec![0u64; AJTAI_N];
    w[0] = (d.amount % Q as u128) as u64;
    for i in 0..(AJTAI_N - 1) {
        w[i + 1] = u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as u64 % Q;
    }
    w
}

/// Fold every deposit this epoch into one constant-size attestation. Size is
/// independent of how many deposits — the bridge publishes one fixed object
/// per epoch instead of N proofs.
pub fn fold_epoch_deposits(deposits: &[DepositRecord]) -> FoldedProof {
    let ajtai = Ajtai::from_seed(AJTAI_M, AJTAI_N, &BRIDGE_FOLD_SEED);
    let witnesses: Vec<Vec<u64>> = deposits.iter().map(witness).collect();
    fold(&ajtai, &witnesses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(amount: u128, who: &str) -> DepositRecord {
        DepositRecord { asset: BridgeAsset::Btc, amount, recipient: who.into(), source_block_hash: [amount as u8; 32] }
    }

    #[test]
    fn epoch_attestation_is_constant_size() {
        let small = fold_epoch_deposits(&[dep(100, "a"), dep(200, "b")]).size_bytes();
        let many: Vec<DepositRecord> = (0..300).map(|i| dep(i, &format!("m{i}"))).collect();
        assert_eq!(small, fold_epoch_deposits(&many).size_bytes(), "one fixed-size attestation per epoch, ∀ deposit count");
    }

    #[test]
    fn changing_a_deposit_moves_the_attestation() {
        let a = fold_epoch_deposits(&[dep(100, "a")]);
        let b = fold_epoch_deposits(&[dep(101, "a")]);
        assert_ne!((a.c_star, a.w_star), (b.c_star, b.w_star));
    }
}
