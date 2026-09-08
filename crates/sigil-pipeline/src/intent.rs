//! **Intent layer** — what the user wants, as one signed-able object, independent of how it
//! is executed. The pipeline stops deciding "BTC → wSIGIL3" and starts executing this.

use serde::{Deserialize, Serialize};

use crate::eth_out::{eth_address_hex, parse_eth_address};
use crate::external::{AssetId, ExternalChain};
use crate::PipelineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Privacy {
    /// Value crosses SIGIL as a hidden note under a STARK; the destination is sealed.
    ShieldedTransit,
}

/// A cross-chain intent. `intent_id` is content-derived, so the same wish twice is two
/// intents only if something (nonce) differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossChainIntent {
    pub source: ExternalChain,
    pub input: AssetId,
    pub amount_in: u64,
    pub destination: ExternalChain,
    pub output: AssetId,
    /// Least acceptable output, in the output asset's base unit. The solver refuses any
    /// route that cannot clear it — slippage is bounded by the user, not by the market.
    pub min_output: u128,
    pub recipient: [u8; 20],
    pub privacy: Privacy,
    /// Unix seconds after which no leg may execute.
    pub deadline: u64,
    pub nonce: u64,
}

impl CrossChainIntent {
    pub fn intent_id(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-pipeline/intent/v1");
        h.update(&serde_json::to_vec(self).expect("serialisable"));
        *h.finalize().as_bytes()
    }

    /// The v1 memo sealed into the vault-bound note. Bitcoin never sees it; SIGIL never
    /// sees it in the clear; the vault opens it and learns exactly what it needs to settle.
    pub fn exit_memo(&self) -> ExitIntentV1 {
        ExitIntentV1 {
            chain: self.destination,
            recipient: self.recipient,
            asset: self.output,
            min_amount: self.min_output,
            intent_id: self.intent_id(),
            deadline: self.deadline,
        }
    }
}

/// `SIGILX-exit/v1|chain=…|to=0x…|asset=…|min=…|intent=<hex32>|deadline=…`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitIntentV1 {
    pub chain: ExternalChain,
    pub recipient: [u8; 20],
    pub asset: AssetId,
    pub min_amount: u128,
    pub intent_id: [u8; 32],
    pub deadline: u64,
}

fn chain_name(c: ExternalChain) -> &'static str {
    match c {
        ExternalChain::Bitcoin => "bitcoin",
        ExternalChain::Lightning => "lightning",
        ExternalChain::Ethereum => "ethereum",
        ExternalChain::Polygon => "polygon",
    }
}
fn chain_from(s: &str) -> Option<ExternalChain> {
    Some(match s {
        "bitcoin" => ExternalChain::Bitcoin,
        "lightning" => ExternalChain::Lightning,
        "ethereum" => ExternalChain::Ethereum,
        "polygon" => ExternalChain::Polygon,
        _ => return None,
    })
}
fn asset_from(s: &str) -> Option<AssetId> {
    Some(match s {
        "BTC" => AssetId::Btc,
        "SIGIL" => AssetId::Sigil,
        "wSIGIL" => AssetId::WSigil,
        "USDC" => AssetId::Usdc,
        "ETH" => AssetId::NativeEvm,
        _ => return None,
    })
}

impl ExitIntentV1 {
    pub const MEMO_PREFIX: &'static str = "SIGILX-exit/v1";

    pub fn encode(&self) -> String {
        format!(
            "{}|chain={}|to={}|asset={}|min={}|intent={}|deadline={}",
            Self::MEMO_PREFIX,
            chain_name(self.chain),
            eth_address_hex(&self.recipient),
            self.asset.symbol(),
            self.min_amount,
            hex::encode(self.intent_id),
            self.deadline
        )
    }

    pub fn decode(memo: &str) -> Result<Self, PipelineError> {
        let bad = |m: &str| PipelineError::BadIntent(m.to_string());
        let mut it = memo.trim_end_matches('\0').split('|');
        if it.next() != Some(Self::MEMO_PREFIX) {
            return Err(bad("missing v1 prefix"));
        }
        let (mut chain, mut to, mut asset, mut min, mut intent, mut deadline) = (None, None, None, None, None, None);
        for part in it {
            let (k, v) = part.split_once('=').ok_or_else(|| bad("field without ="))?;
            match k {
                "chain" => chain = Some(chain_from(v).ok_or_else(|| bad("unknown chain"))?),
                "to" => to = Some(parse_eth_address(v)?),
                "asset" => asset = Some(asset_from(v).ok_or_else(|| bad("unknown asset"))?),
                "min" => min = Some(v.parse::<u128>().map_err(|e| bad(&e.to_string()))?),
                "intent" => {
                    let b = hex::decode(v).map_err(|e| bad(&e.to_string()))?;
                    intent = Some(<[u8; 32]>::try_from(b).map_err(|_| bad("intent id is 32 bytes"))?);
                }
                "deadline" => deadline = Some(v.parse::<u64>().map_err(|e| bad(&e.to_string()))?),
                _ => {} // forward-compatible: unknown fields are ignored, never fatal
            }
        }
        Ok(Self {
            chain: chain.ok_or_else(|| bad("no chain"))?,
            recipient: to.ok_or_else(|| bad("no recipient"))?,
            asset: asset.ok_or_else(|| bad("no asset"))?,
            min_amount: min.ok_or_else(|| bad("no min"))?,
            intent_id: intent.ok_or_else(|| bad("no intent id"))?,
            deadline: deadline.ok_or_else(|| bad("no deadline"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CrossChainIntent {
        CrossChainIntent {
            source: ExternalChain::Bitcoin,
            input: AssetId::Btc,
            amount_in: 10_000_000,
            destination: ExternalChain::Polygon,
            output: AssetId::Usdc,
            min_output: 1_000_000,
            recipient: [0xd7; 20],
            privacy: Privacy::ShieldedTransit,
            deadline: 1_800_000_000,
            nonce: 1,
        }
    }

    #[test]
    fn intent_id_is_content_derived_and_nonce_separated() {
        let a = sample();
        let mut b = sample();
        assert_eq!(a.intent_id(), b.intent_id());
        b.nonce = 2;
        assert_ne!(a.intent_id(), b.intent_id());
    }

    #[test]
    fn v1_memo_round_trips_and_fits_the_sealed_memo_budget() {
        let m = sample().exit_memo();
        let s = m.encode();
        assert!(s.len() <= sigil_shield::note_cipher::MEMO_LEN, "memo {} B > {}", s.len(), sigil_shield::note_cipher::MEMO_LEN);
        assert_eq!(ExitIntentV1::decode(&s).unwrap(), m);
        assert!(ExitIntentV1::decode("SIGILX-exit/v0|chain=137|eth=0x00").is_err(), "v0 is not v1");
        assert!(ExitIntentV1::decode(&s.replace("|min=1000000", "")).is_err());
    }
}
