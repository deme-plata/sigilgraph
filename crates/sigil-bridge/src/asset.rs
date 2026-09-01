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

#[cfg(test)]
mod asset_tests {
    use super::BridgeAsset;

    #[test]
    fn tags_are_stable_and_distinct() {
        // These 1-byte tags feed the committed bridge supply-root hash. Changing
        // one, or letting two assets collide on the same tag, silently forks the
        // root across nodes — a consensus fault. Pin the exact values.
        assert_eq!(BridgeAsset::Btc.tag(), 1);
        assert_eq!(BridgeAsset::Eth.tag(), 2);
        assert_eq!(BridgeAsset::Zec.tag(), 3);
        assert_eq!(BridgeAsset::Iron.tag(), 4);

        let mut tags: Vec<u8> = BridgeAsset::all().iter().map(|a| a.tag()).collect();
        let n = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), n, "asset tags must be unique (a collision merges root slots)");
    }

    #[test]
    fn wrapped_symbols_are_distinct() {
        // Symbols name the on-chain wrapped token; a duplicate would alias two
        // different collateral pools onto one visible token.
        let mut syms: Vec<&str> = BridgeAsset::all().iter().map(|a| a.wrapped_symbol()).collect();
        let n = syms.len();
        syms.sort_unstable();
        syms.dedup();
        assert_eq!(syms.len(), n, "wrapped symbols must be unique");
    }

    #[test]
    fn every_asset_needs_at_least_one_confirmation() {
        // A zero-confirmation asset would let a mint fire on an unconfirmed,
        // reorg-able deposit — the classic bridge double-spend. Never allow 0.
        for a in BridgeAsset::all() {
            assert!(a.min_confirmations() >= 1, "{a:?} must require >= 1 confirmation");
        }
    }
}
