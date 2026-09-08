//! **External Proof Layer** — verified external state enters SIGIL through ONE shape.
//!
//! Bitcoin is the first and most carefully hardened source, not the last. Every source
//! chain contributes a proof variant and a verifier; consensus sees only
//! [`ExternalTransition`] `{ proof, commitment }`, so adding a chain never adds a
//! transaction type. A variant with no real verifier is refused, never faked.

use sigil_bridge::{BridgeAsset, BridgeLedger, LnProof, SpvProof};

use crate::btc_in::{admit_deposit, BtcPolicy, VerifiedBtcDeposit};
use crate::PipelineError;

/// The external state machines SIGIL can admit value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ExternalChain {
    Bitcoin,
    Lightning,
    Ethereum,
    Polygon,
}

/// The canonical asset layer: one id per economic asset, whatever chain it sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AssetId {
    /// Native bitcoin, in satoshis.
    Btc,
    /// Native SIGIL, in glyphs (10dp).
    Sigil,
    /// Wrapped SIGIL on an EVM chain (18dp).
    WSigil,
    /// USDC on an EVM chain (6dp).
    Usdc,
    /// Native ether / POL on an EVM chain (18dp).
    NativeEvm,
}

impl AssetId {
    pub fn decimals(self) -> u32 {
        match self {
            AssetId::Btc => 8,
            AssetId::Sigil => 10,
            AssetId::WSigil => 18,
            AssetId::Usdc => 6,
            AssetId::NativeEvm => 18,
        }
    }
    pub fn symbol(self) -> &'static str {
        match self {
            AssetId::Btc => "BTC",
            AssetId::Sigil => "SIGIL",
            AssetId::WSigil => "wSIGIL",
            AssetId::Usdc => "USDC",
            AssetId::NativeEvm => "ETH",
        }
    }
}

/// A proof that something happened on an external chain. Each variant carries the exact
/// evidence its verifier needs and nothing a caller could substitute.
#[derive(Debug, Clone)]
pub enum ExternalProof {
    /// Bitcoin: a tx buried under PoW headers — see [`sigil_bridge::SpvProof`].
    BitcoinSpv(SpvProof),
    /// Lightning: a settled BOLT11 invoice with the payer's preimage — see [`sigil_bridge::LnProof`].
    /// Unlike the Bitcoin memo, a BOLT11 invoice does not bind the SIGIL recipient, so the
    /// shield key is carried beside the proof and the SUBMITTER is trusted for it. Amount and
    /// payment hash are bound by the invoice signature.
    Lightning { proof: LnProof, shield_pk: [u8; 32] },
    /// Ethereum receipt/log proofs are NOT implemented: an honest verifier needs a receipt-trie
    /// MPT proof plus a finality argument for the header, and neither exists in this tree.
    /// Kept as a variant so the shape is stable; `verify` refuses it.
    EthereumReceipt { tx_hash: [u8; 32] },
}

impl ExternalProof {
    pub fn chain(&self) -> ExternalChain {
        match self {
            ExternalProof::BitcoinSpv(_) => ExternalChain::Bitcoin,
            ExternalProof::Lightning { .. } => ExternalChain::Lightning,
            ExternalProof::EthereumReceipt { .. } => ExternalChain::Ethereum,
        }
    }
    pub fn asset(&self) -> AssetId {
        match self {
            ExternalProof::BitcoinSpv(_) | ExternalProof::Lightning { .. } => AssetId::Btc,
            ExternalProof::EthereumReceipt { .. } => AssetId::NativeEvm,
        }
    }
}

/// Admission policy per source. Only the sources with a real verifier have fields.
#[derive(Debug, Clone)]
pub struct ExternalPolicy {
    pub bitcoin: BtcPolicy,
}

impl ExternalPolicy {
    pub fn mainnet() -> Self {
        Self { bitcoin: BtcPolicy::mainnet() }
    }
    pub fn synthetic() -> Self {
        Self { bitcoin: BtcPolicy::synthetic() }
    }
}

/// What every verifier must produce: the facts the private leg needs, bound to the proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExternalDeposit {
    pub source: ExternalChain,
    pub asset: AssetId,
    /// Unique per deposit on its source chain (txid, payment hash, …) — the replay key.
    pub source_id: [u8; 32],
    pub amount: u64,
    /// The SIGIL shield key the deposit named. The only SIGIL identity the source learns.
    pub shield_pk: [u8; 32],
    pub supply_root: [u8; 32],
    /// Source-chain finality evidence, in the source's own unit (confirmations, or 0 for
    /// an instantly-final rail like Lightning).
    pub finality: u32,
}

impl From<VerifiedBtcDeposit> for VerifiedExternalDeposit {
    fn from(d: VerifiedBtcDeposit) -> Self {
        Self {
            source: ExternalChain::Bitcoin,
            asset: AssetId::Btc,
            source_id: d.txid,
            amount: d.sats,
            shield_pk: d.shield_pk,
            supply_root: d.supply_root,
            finality: d.confirmations,
        }
    }
}

/// The one consensus-facing shape: proof in, hidden commitment out. `commitment` is the
/// note leaf the depositor will own; consensus checks it is owner-bound to the shield key the
/// proof names (the private leg does that binding — see `private_mid::mint_deposit_note`).
#[derive(Debug, Clone)]
pub struct ExternalTransition {
    pub proof: ExternalProof,
    pub commitment: [u8; 32],
}

/// Verify an external proof under `policy` and, only then, lock + mint under the peg
/// chokepoint. Sources without a verifier are refused here, loudly.
pub fn verify_external(
    ledger: &mut BridgeLedger,
    proof: &ExternalProof,
    policy: &ExternalPolicy,
) -> Result<VerifiedExternalDeposit, PipelineError> {
    match proof {
        ExternalProof::BitcoinSpv(spv) => Ok(admit_deposit(ledger, spv, &policy.bitcoin)?.into()),
        ExternalProof::Lightning { proof, shield_pk } => {
            let v = proof.verify(None).map_err(|e| PipelineError::Btc(e.into()))?;
            let receipt = sigil_bridge::process_ln_deposit(ledger, hex::encode(shield_pk), proof, None)?;
            let mut source_id = [0u8; 32];
            let ph: &[u8] = v.payment_hash.as_ref();
            if ph.len() != 32 {
                return Err(PipelineError::BadIntent("payment hash is not 32 bytes".into()));
            }
            source_id.copy_from_slice(ph);
            Ok(VerifiedExternalDeposit {
                source: ExternalChain::Lightning,
                asset: AssetId::Btc,
                source_id,
                amount: u64::try_from(receipt.amount).map_err(|_| PipelineError::DepositTooLarge { sats: u64::MAX })?,
                shield_pk: *shield_pk,
                supply_root: receipt.supply_root,
                finality: 0,
            })
        }
        ExternalProof::EthereumReceipt { .. } => Err(PipelineError::Unsupported(
            "Ethereum receipt proofs have no verifier in this tree (needs receipt-trie MPT + header finality); refusing rather than trusting".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btc_in::synthetic;

    #[test]
    fn bitcoin_spv_flows_through_the_generic_shape() {
        let proof = ExternalProof::BitcoinSpv(synthetic::deposit_proof(7_000, [3u8; 32], 6));
        assert_eq!(proof.chain(), ExternalChain::Bitcoin);
        assert_eq!(proof.asset(), AssetId::Btc);
        let d = verify_external(&mut BridgeLedger::new(), &proof, &ExternalPolicy::synthetic()).unwrap();
        assert_eq!(d.amount, 7_000);
        assert_eq!(d.shield_pk, [3u8; 32]);
        assert_eq!(d.finality, 6);
    }

    #[test]
    fn an_unimplemented_source_is_refused_not_faked() {
        let proof = ExternalProof::EthereumReceipt { tx_hash: [0xEE; 32] };
        assert!(matches!(
            verify_external(&mut BridgeLedger::new(), &proof, &ExternalPolicy::synthetic()),
            Err(PipelineError::Unsupported(_))
        ));
    }

    #[test]
    fn asset_decimals_are_the_live_ones() {
        assert_eq!(AssetId::Sigil.decimals(), 10);
        assert_eq!(AssetId::WSigil.decimals(), 18);
        assert_eq!(AssetId::Btc.decimals(), 8);
        assert_eq!(AssetId::Usdc.decimals(), 6);
    }
}
