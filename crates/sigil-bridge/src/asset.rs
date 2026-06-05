//! asset.rs — the bridged assets and their per-chain finality rules.

use serde::{Deserialize, Serialize};

/// A source-chain asset SIGIL wraps. One generic enum for all coins (vs
/// Quillon's per-chain crate sprawl: q-bitcoin-bridge / q-zcash-bridge / …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BridgeAsset {
    Btc,
    Eth,
    Zec,
    Iron,
}

impl BridgeAsset {
    /// The wrapped SIGIL token symbol.
    pub fn wrapped_symbol(self) -> &'static str {
        match self {
            BridgeAsset::Btc => "wBTC",
            BridgeAsset::Eth => "wETH",
            BridgeAsset::Zec => "wZEC",
            BridgeAsset::Iron => "wIRON",
        }
    }

    /// Confirmations required before a deposit is final enough to mint. Tuned to
    /// each chain's reorg risk (Quillon used a flat 6 for BTC; we set per-chain).
    pub fn min_confirmations(self) -> u32 {
        match self {
            BridgeAsset::Btc => 6,
            BridgeAsset::Zec => 10,
            BridgeAsset::Eth => 32, // ~ finalized
            BridgeAsset::Iron => 6,
        }
    }

    /// Stable 1-byte discriminant for the supply-root hash (never re-purpose).
    pub fn tag(self) -> u8 {
        match self {
            BridgeAsset::Btc => 1,
            BridgeAsset::Eth => 2,
            BridgeAsset::Zec => 3,
            BridgeAsset::Iron => 4,
        }
    }

    /// All assets — for iterating the full bridge surface.
    pub fn all() -> [BridgeAsset; 4] {
        [BridgeAsset::Btc, BridgeAsset::Eth, BridgeAsset::Zec, BridgeAsset::Iron]
    }
}
