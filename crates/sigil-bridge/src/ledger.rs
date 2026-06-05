//! ledger.rs — the bridge supply ledger + the committed peg invariant.
//!
//! Quillon's wrapped-supply lives off to the side of consensus (its bridge
//! status returns `?`/opaque, and a dead daemon hides the real numbers). SIGIL
//! commits a **bridge-supply root** over every asset's `(locked, minted)` into
//! each block, and the mint chokepoint ENFORCES `minted ≤ locked`. Over-minting
//! a wrapped token is therefore both impossible (the guard) and unhideable (the
//! root) — the SIGIL anti-Quillon-failure discipline applied to the bridge.

use crate::asset::BridgeAsset;
use std::collections::BTreeMap;

/// Per-asset peg state: source-chain collateral locked vs wrapped minted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetPeg {
    /// Source-chain units locked (e.g. BTC satoshis) — only grows on a verified
    /// deposit, only shrinks on a verified withdrawal.
    pub locked: u128,
    /// Wrapped units currently minted on SIGIL.
    pub minted: u128,
}

/// Why a ledger mutation was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PegError {
    #[error("mint would break the peg: minted {minted} > locked {locked} for {asset:?}")]
    ExceedsCollateral { asset: BridgeAsset, minted: u128, locked: u128 },
    #[error("burn/unlock exceeds balance for {asset:?}")]
    Underflow { asset: BridgeAsset },
    #[error("arithmetic overflow")]
    Overflow,
}

/// The bridge's full supply ledger. `BTreeMap` so the root is deterministic.
#[derive(Debug, Clone, Default)]
pub struct BridgeLedger {
    entries: BTreeMap<BridgeAsset, AssetPeg>,
}

impl BridgeLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn peg(&self, asset: BridgeAsset) -> AssetPeg {
        self.entries.get(&asset).copied().unwrap_or_default()
    }

    /// Record source-chain collateral locked (called only after an SPV proof
    /// verified the deposit).
    pub fn lock(&mut self, asset: BridgeAsset, amount: u128) -> Result<(), PegError> {
        let e = self.entries.entry(asset).or_default();
        e.locked = e.locked.checked_add(amount).ok_or(PegError::Overflow)?;
        Ok(())
    }

    /// Mint wrapped tokens — the chokepoint. ENFORCES `minted ≤ locked`.
    pub fn mint(&mut self, asset: BridgeAsset, amount: u128) -> Result<(), PegError> {
        let e = self.entries.entry(asset).or_default();
        let new_minted = e.minted.checked_add(amount).ok_or(PegError::Overflow)?;
        if new_minted > e.locked {
            return Err(PegError::ExceedsCollateral { asset, minted: new_minted, locked: e.locked });
        }
        e.minted = new_minted;
        Ok(())
    }

    /// Burn wrapped tokens on withdrawal (then `unlock` releases the collateral).
    pub fn burn(&mut self, asset: BridgeAsset, amount: u128) -> Result<(), PegError> {
        let e = self.entries.entry(asset).or_default();
        e.minted = e.minted.checked_sub(amount).ok_or(PegError::Underflow { asset })?;
        Ok(())
    }

    /// Release locked collateral after a burn (paying out the withdrawal).
    pub fn unlock(&mut self, asset: BridgeAsset, amount: u128) -> Result<(), PegError> {
        let e = self.entries.entry(asset).or_default();
        e.locked = e.locked.checked_sub(amount).ok_or(PegError::Underflow { asset })?;
        Ok(())
    }

    /// Is every asset's peg sound (`minted ≤ locked`)? A node asserts this each
    /// block; a false here is a consensus-halting bridge fault.
    pub fn peg_ok(&self) -> bool {
        self.entries.values().all(|p| p.minted <= p.locked)
    }

    /// The committed bridge-supply root for this block — BLAKE3 over every
    /// asset's `(tag, locked, minted)` in asset order. Goes into the SIGIL
    /// header alongside the other state roots; anyone can recompute it and check
    /// the peg without trusting the bridge operator.
    pub fn root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-bridge/supply-root/v1");
        for (asset, peg) in &self.entries {
            h.update(&[asset.tag()]);
            h.update(&peg.locked.to_le_bytes());
            h.update(&peg.minted.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_within_collateral_ok_over_fails() {
        let mut l = BridgeLedger::new();
        l.lock(BridgeAsset::Btc, 100).unwrap();
        assert!(l.mint(BridgeAsset::Btc, 100).is_ok());
        // one more wrapped unit than collateral → rejected
        assert_eq!(
            l.mint(BridgeAsset::Btc, 1),
            Err(PegError::ExceedsCollateral { asset: BridgeAsset::Btc, minted: 101, locked: 100 })
        );
        assert!(l.peg_ok());
    }

    #[test]
    fn root_is_deterministic_and_changes_on_mint() {
        let mut a = BridgeLedger::new();
        a.lock(BridgeAsset::Btc, 50).unwrap();
        a.mint(BridgeAsset::Btc, 50).unwrap();
        let mut b = BridgeLedger::new();
        b.lock(BridgeAsset::Btc, 50).unwrap();
        b.mint(BridgeAsset::Btc, 50).unwrap();
        assert_eq!(a.root(), b.root(), "same state → same root");

        let before = a.root();
        a.lock(BridgeAsset::Eth, 10).unwrap();
        a.mint(BridgeAsset::Eth, 10).unwrap();
        assert_ne!(before, a.root(), "new asset mint must move the root");
    }

    #[test]
    fn withdraw_burns_then_unlocks_keeping_peg() {
        let mut l = BridgeLedger::new();
        l.lock(BridgeAsset::Zec, 30).unwrap();
        l.mint(BridgeAsset::Zec, 30).unwrap();
        l.burn(BridgeAsset::Zec, 10).unwrap();
        l.unlock(BridgeAsset::Zec, 10).unwrap();
        assert_eq!(l.peg(BridgeAsset::Zec), AssetPeg { locked: 20, minted: 20 });
        assert!(l.peg_ok());
    }
}
