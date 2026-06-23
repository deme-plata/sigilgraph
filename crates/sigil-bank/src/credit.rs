//! SIGIL collateral-credit vault — the LANE-X port of Quillon's QCREDIT
//! (`q-api-server/src/qcredit_api.rs` + `q-vm/contracts/qcredit_vault.rs`).
//!
//! Quillon locked QUG 1:1 → minted QCREDIT. SIGIL is conservative: lock SIGIL
//! collateral → mint CREDIT at **50% LTV** ([`LTV_BPS`]), so every CREDIT unit
//! is over-collateralized 2:1 in SIGIL terms. No price oracle is needed —
//! collateral and credit are both SIGIL-denominated, so the LTV is exact by
//! construction, not by market mark.
//!
//! Flows (mirrors the QCREDIT endpoint surface):
//!   - **lock**: deposit SIGIL collateral for a tier's term → mint
//!     `amount * LTV` CREDIT. Collateral moves to the vault wallet
//!     ([`CREDIT_VAULT_WALLET`]) via the chokepoint; the vault only does math.
//!   - **claim**: claim accrued yield (tiered APY on the collateral) without
//!     unlocking — paid from [`CreditVault::protocol_reserve`], capped by it.
//!   - **unlock**: after term expiry, burn the full minted CREDIT → collateral
//!     returns + pending yield.
//!   - **liquidate** (breach): term expired + the wallet can no longer return
//!     the minted CREDIT → the bank pool takes the collateral, position closes.
//!     At 50% LTV the bank is never underwater: it seizes 2 SIGIL of collateral
//!     for every 1 CREDIT left circulating.
//!
//! Same contract as the rest of sigil-bank: **no I/O, no state borrow** — the
//! caller (sigil-rpcd) threads every balance mutation through
//! `sigil_state::commit_state_transition`, and persists this struct in its own
//! flux-db key so a restart NEVER loses a loan (the LANE-X acceptance gate).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::WalletId;

/// 32-byte token id — mirrors `sigil_state::TokenId` without importing
/// sigil-state (same one-way-dep rule as [`crate::WalletId`]).
pub type TokenId = [u8; 32];

/// The CREDIT token id (legible byte-fill, same convention as the rpcd
/// predefined tokens — 0xAA=USDS, 0xBB=wQUG, …). Minted only by the vault.
pub const CREDIT_TOKEN: TokenId = [0xCD; 32];

/// The vault custody wallet: locked SIGIL collateral + the funded yield
/// reserve live here. Only the credit routes move funds in/out, and every
/// move goes through the chokepoint.
pub const CREDIT_VAULT_WALLET: WalletId = [0xCF; 32];

/// Loan-to-value in basis points: CREDIT minted per unit of SIGIL locked.
/// **5000 bps = 50%** — the conservative start the LANE-X spec mandates.
/// One knob, shared by every tier; a future consensus-gated change can raise
/// it (or make it per-tier) once the credit market has history.
pub const LTV_BPS: u128 = 5_000;

/// Basis-point denominator (10_000 = 100%), same convention as the fee splits.
pub const BPS: u128 = 10_000;

/// Seconds in a 365-day year, for APY accrual.
const SECONDS_PER_YEAR: u64 = 365 * 24 * 3600;

// ── Tier parameters — conservative SIGIL restating of the QCREDIT tiers ──
// Quillon ran 5/10/15/25% APY. SIGIL starts at roughly half: the reserve is
// small and yield must never outrun what the bank actually earns in fees.
const BRONZE_LOCK_SECONDS: u64 = 7 * 24 * 3600;
const SILVER_LOCK_SECONDS: u64 = 30 * 24 * 3600;
const GOLD_LOCK_SECONDS: u64 = 90 * 24 * 3600;
const PLATINUM_LOCK_SECONDS: u64 = 180 * 24 * 3600;

const BRONZE_APY_BPS: u64 = 300; // 3%
const SILVER_APY_BPS: u64 = 500; // 5%
const GOLD_APY_BPS: u64 = 800; // 8%
const PLATINUM_APY_BPS: u64 = 1_200; // 12%

/// Grace period after term expiry before a position becomes liquidatable.
/// 7 days: enough for an operator to top up CREDIT and unlock cleanly.
pub const LIQUIDATION_GRACE_SECONDS: u64 = 7 * 24 * 3600;

/// Credit tier: lock duration + yield rate. Same four names as QCREDIT so
/// wallet UIs port over unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CreditTier {
    /// 7-day lock, 3% APY.
    Bronze,
    /// 30-day lock, 5% APY.
    Silver,
    /// 90-day lock, 8% APY.
    Gold,
    /// 180-day lock, 12% APY.
    Platinum,
}

impl CreditTier {
    /// Parse a tier from its lowercase request name.
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "bronze" => Some(Self::Bronze),
            "silver" => Some(Self::Silver),
            "gold" => Some(Self::Gold),
            "platinum" => Some(Self::Platinum),
            _ => None,
        }
    }

    /// Lock duration in seconds.
    pub fn lock_duration(&self) -> u64 {
        match self {
            Self::Bronze => BRONZE_LOCK_SECONDS,
            Self::Silver => SILVER_LOCK_SECONDS,
            Self::Gold => GOLD_LOCK_SECONDS,
            Self::Platinum => PLATINUM_LOCK_SECONDS,
        }
    }

    /// Yield rate in basis points per year.
    pub fn apy_bps(&self) -> u64 {
        match self {
            Self::Bronze => BRONZE_APY_BPS,
            Self::Silver => SILVER_APY_BPS,
            Self::Gold => GOLD_APY_BPS,
            Self::Platinum => PLATINUM_APY_BPS,
        }
    }

    /// Display name (matches the request name, capitalized).
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Bronze => "Bronze",
            Self::Silver => "Silver",
            Self::Gold => "Gold",
            Self::Platinum => "Platinum",
        }
    }

    /// All tiers, for status/tier listings.
    pub fn all_tiers() -> &'static [CreditTier] {
        &[Self::Bronze, Self::Silver, Self::Gold, Self::Platinum]
    }
}

/// One open credit position: SIGIL collateral locked, CREDIT minted at LTV.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditPosition {
    /// Owner wallet (hex64-decoded id).
    pub wallet: WalletId,
    /// SIGIL collateral locked (base units).
    pub collateral_locked: u128,
    /// CREDIT minted = `collateral_locked * LTV_BPS / BPS` (floor — rounding
    /// favors the bank, mirroring the fee-split convention).
    pub credit_minted: u128,
    /// The tier this position was opened under.
    pub tier: CreditTier,
    /// Unix seconds when the position opened.
    pub lock_timestamp: u64,
    /// Unix seconds when unlock becomes available.
    pub unlock_timestamp: u64,
    /// Yield already paid out (base units).
    pub claimed_yield: u128,
    /// Last yield-claim time (accrual restarts from here).
    pub last_claim_timestamp: u64,
}

impl CreditPosition {
    /// Unclaimed yield accrued since the last claim:
    /// `collateral * apy_bps / 10_000 * elapsed / year`, floored.
    pub fn pending_yield(&self, now: u64) -> u128 {
        let elapsed = now.saturating_sub(self.last_claim_timestamp);
        if elapsed == 0 || self.collateral_locked == 0 {
            return 0;
        }
        self.collateral_locked
            .checked_mul(self.tier.apy_bps() as u128)
            .and_then(|v| v.checked_mul(elapsed as u128))
            .map(|v| v / (BPS * SECONDS_PER_YEAR as u128))
            .unwrap_or(0)
    }

    /// Whether the lock term has expired (unlock allowed).
    pub fn is_unlockable(&self, now: u64) -> bool {
        now >= self.unlock_timestamp
    }

    /// Whether the position has breached: term + grace expired without an
    /// unlock. A breached position may be liquidated — the bank pool takes
    /// the collateral.
    pub fn is_breached(&self, now: u64) -> bool {
        now >= self.unlock_timestamp.saturating_add(LIQUIDATION_GRACE_SECONDS)
    }
}

/// Tier info row for status/tier listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierInfo {
    /// The tier.
    pub tier: CreditTier,
    /// Lock term in whole days.
    pub lock_days: u64,
    /// Yield rate as a percentage.
    pub apy_percent: f64,
    /// Loan-to-value as a percentage (CREDIT minted per SIGIL locked).
    pub ltv_percent: f64,
}

/// Vault-wide status summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultStatus {
    /// Total SIGIL collateral locked across all positions.
    pub total_collateral: u128,
    /// Total CREDIT in circulation (minted, not yet burned).
    pub total_credit_supply: u128,
    /// Yield reserve available for payouts.
    pub protocol_reserve: u128,
    /// Cumulative yield paid out over the vault's life.
    pub total_yield_paid: u128,
    /// Cumulative collateral seized by liquidation.
    pub total_liquidated: u128,
    /// Open position count.
    pub position_count: usize,
    /// The tier table.
    pub tiers: Vec<TierInfo>,
}

/// Outcome of [`CreditVault::unlock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlockOutcome {
    /// SIGIL collateral returned to the wallet.
    pub collateral_returned: u128,
    /// CREDIT the caller must burn from the wallet (== the position's mint).
    pub credit_burned: u128,
    /// Yield paid alongside (capped by the reserve).
    pub yield_paid: u128,
}

/// Outcome of [`CreditVault::liquidate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationOutcome {
    /// SIGIL collateral seized by the bank pool.
    pub collateral_seized: u128,
    /// CREDIT left circulating, now backed by the seized collateral on the
    /// bank's books (2:1 at the 50% LTV start — the bank is never underwater).
    pub credit_outstanding: u128,
}

/// The collateral-credit vault. Pure state machine: serde round-trips
/// byte-exactly (bincode), so it can live in its OWN flux-db key — never
/// inside the rpcd `Snapshot` struct (bincode is positional; appending a
/// field there breaks decode of pre-existing production snapshots, which is
/// exactly the restart-reset failure LANE-X exists to prevent).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreditVault {
    /// wallet → open positions.
    pub positions: HashMap<WalletId, Vec<CreditPosition>>,
    /// Yield reserve (SIGIL base units, backed 1:1 by NATIVE in the vault
    /// wallet). Funded by bank fees; yield is capped by it.
    pub protocol_reserve: u128,
    /// Total SIGIL collateral locked.
    pub total_collateral: u128,
    /// Total CREDIT minted and circulating.
    pub total_credit_supply: u128,
    /// Cumulative yield paid.
    pub total_yield_paid: u128,
    /// Cumulative collateral seized via liquidation.
    pub total_liquidated: u128,
}

impl CreditVault {
    /// Fresh, empty vault.
    pub fn new() -> Self {
        Self::default()
    }

    /// CREDIT minted for a given collateral amount: `floor(amount * LTV)`.
    pub fn credit_for_collateral(amount: u128) -> Option<u128> {
        amount.checked_mul(LTV_BPS).map(|v| v / BPS)
    }

    /// The tier table for display.
    pub fn get_tiers() -> Vec<TierInfo> {
        CreditTier::all_tiers()
            .iter()
            .map(|t| TierInfo {
                tier: *t,
                lock_days: t.lock_duration() / 86_400,
                apy_percent: t.apy_bps() as f64 / 100.0,
                ltv_percent: LTV_BPS as f64 / 100.0,
            })
            .collect()
    }

    /// Vault-wide status summary.
    pub fn status(&self) -> VaultStatus {
        VaultStatus {
            total_collateral: self.total_collateral,
            total_credit_supply: self.total_credit_supply,
            protocol_reserve: self.protocol_reserve,
            total_yield_paid: self.total_yield_paid,
            total_liquidated: self.total_liquidated,
            position_count: self.positions.values().map(|v| v.len()).sum(),
            tiers: Self::get_tiers(),
        }
    }

    /// Open a position: lock `amount` SIGIL collateral, mint CREDIT at LTV.
    ///
    /// Returns the created position. The CALLER (through the chokepoint):
    ///   1. moves `amount` NATIVE wallet → [`CREDIT_VAULT_WALLET`]
    ///   2. credits `position.credit_minted` [`CREDIT_TOKEN`] to the wallet
    pub fn lock(
        &mut self,
        wallet: WalletId,
        amount: u128,
        tier: CreditTier,
        now: u64,
    ) -> Result<CreditPosition, String> {
        if amount == 0 {
            return Err("Amount must be > 0".into());
        }
        let credit = Self::credit_for_collateral(amount).ok_or("collateral overflow")?;
        if credit == 0 {
            return Err(format!("Amount too small: {amount} SIGIL mints 0 CREDIT at 50% LTV"));
        }
        let position = CreditPosition {
            wallet,
            collateral_locked: amount,
            credit_minted: credit,
            tier,
            lock_timestamp: now,
            unlock_timestamp: now + tier.lock_duration(),
            claimed_yield: 0,
            last_claim_timestamp: now,
        };
        self.total_collateral = self.total_collateral.saturating_add(amount);
        self.total_credit_supply = self.total_credit_supply.saturating_add(credit);
        self.positions.entry(wallet).or_default().push(position.clone());
        Ok(position)
    }

    /// Close a position after term expiry: burn the minted CREDIT, return the
    /// collateral + pending yield (capped by the reserve).
    ///
    /// The CALLER must verify the wallet still holds `credit_burned` CREDIT
    /// before committing, then (through the chokepoint):
    ///   1. burns `credit_burned` [`CREDIT_TOKEN`] from the wallet
    ///   2. moves `collateral_returned + yield_paid` NATIVE vault → wallet
    pub fn unlock(
        &mut self,
        wallet: &WalletId,
        position_index: usize,
        now: u64,
    ) -> Result<UnlockOutcome, String> {
        let positions = self.positions.get_mut(wallet).ok_or("No positions found")?;
        if position_index >= positions.len() {
            return Err("Invalid position index".into());
        }
        let pos = &positions[position_index];
        if !pos.is_unlockable(now) {
            return Err(format!(
                "Position locked until {} (now={})",
                pos.unlock_timestamp, now
            ));
        }
        let pending = pos.pending_yield(now);
        let yield_paid = pending.min(self.protocol_reserve);
        let collateral_returned = pos.collateral_locked;
        let credit_burned = pos.credit_minted;

        self.total_collateral = self.total_collateral.saturating_sub(collateral_returned);
        self.total_credit_supply = self.total_credit_supply.saturating_sub(credit_burned);
        self.total_yield_paid = self.total_yield_paid.saturating_add(yield_paid);
        self.protocol_reserve = self.protocol_reserve.saturating_sub(yield_paid);

        positions.remove(position_index);
        if positions.is_empty() {
            self.positions.remove(wallet);
        }
        Ok(UnlockOutcome { collateral_returned, credit_burned, yield_paid })
    }

    /// Claim accrued yield without unlocking. Capped by the reserve.
    /// Caller moves `yield` NATIVE vault → wallet through the chokepoint.
    pub fn claim_yield(
        &mut self,
        wallet: &WalletId,
        position_index: usize,
        now: u64,
    ) -> Result<u128, String> {
        let positions = self.positions.get_mut(wallet).ok_or("No positions found")?;
        if position_index >= positions.len() {
            return Err("Invalid position index".into());
        }
        let pos = &mut positions[position_index];
        let pending = pos.pending_yield(now);
        if pending == 0 {
            return Err("No yield to claim".into());
        }
        let yield_paid = pending.min(self.protocol_reserve);
        if yield_paid == 0 {
            return Err("Protocol reserve is empty — yield accrues but cannot be paid yet".into());
        }
        pos.claimed_yield = pos.claimed_yield.saturating_add(yield_paid);
        pos.last_claim_timestamp = now;
        self.total_yield_paid = self.total_yield_paid.saturating_add(yield_paid);
        self.protocol_reserve = self.protocol_reserve.saturating_sub(yield_paid);
        Ok(yield_paid)
    }

    /// Liquidate a BREACHED position (term + grace expired): the bank pool
    /// seizes the collateral; the minted CREDIT stays circulating, backed
    /// 2:1 by the seized collateral on the bank's books.
    ///
    /// Caller moves `collateral_seized` NATIVE vault → bank wallet through
    /// the chokepoint.
    pub fn liquidate(
        &mut self,
        wallet: &WalletId,
        position_index: usize,
        now: u64,
    ) -> Result<LiquidationOutcome, String> {
        let positions = self.positions.get_mut(wallet).ok_or("No positions found")?;
        if position_index >= positions.len() {
            return Err("Invalid position index".into());
        }
        let pos = &positions[position_index];
        if !pos.is_breached(now) {
            return Err(format!(
                "Position not breached: liquidatable from {} (now={})",
                pos.unlock_timestamp.saturating_add(LIQUIDATION_GRACE_SECONDS),
                now
            ));
        }
        let collateral_seized = pos.collateral_locked;
        let credit_outstanding = pos.credit_minted;

        self.total_collateral = self.total_collateral.saturating_sub(collateral_seized);
        // credit supply stays — those CREDIT tokens still circulate; the bank
        // absorbed the collateral that backs them.
        self.total_liquidated = self.total_liquidated.saturating_add(collateral_seized);

        positions.remove(position_index);
        if positions.is_empty() {
            self.positions.remove(wallet);
        }
        Ok(LiquidationOutcome { collateral_seized, credit_outstanding })
    }

    /// Fund the yield reserve (bank fees flow here). Caller moves the same
    /// amount of NATIVE into the vault wallet through the chokepoint.
    pub fn fund_reserve(&mut self, amount: u128) {
        self.protocol_reserve = self.protocol_reserve.saturating_add(amount);
    }

    /// All positions for a wallet, with pending yield at `now`.
    pub fn positions_with_yield(&self, wallet: &WalletId, now: u64) -> Vec<(CreditPosition, u128)> {
        self.positions
            .get(wallet)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let pending = p.pending_yield(now);
                (p, pending)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: WalletId = [7u8; 32];
    const NOW: u64 = 1_700_000_000;

    #[test]
    fn lock_mints_at_50pct_ltv() {
        let mut v = CreditVault::new();
        let p = v.lock(W, 1_000, CreditTier::Silver, NOW).unwrap();
        assert_eq!(p.collateral_locked, 1_000);
        assert_eq!(p.credit_minted, 500); // 50% LTV
        assert_eq!(p.unlock_timestamp, NOW + SILVER_LOCK_SECONDS);
        assert_eq!(v.total_collateral, 1_000);
        assert_eq!(v.total_credit_supply, 500);
    }

    #[test]
    fn ltv_rounding_favors_bank() {
        // 3 * 5000 / 10000 = 1.5 → floor 1 (never rounds the mint up)
        assert_eq!(CreditVault::credit_for_collateral(3).unwrap(), 1);
        assert_eq!(CreditVault::credit_for_collateral(1).unwrap(), 0);
    }

    #[test]
    fn dust_lock_rejected() {
        let mut v = CreditVault::new();
        assert!(v.lock(W, 0, CreditTier::Bronze, NOW).is_err());
        assert!(v.lock(W, 1, CreditTier::Bronze, NOW).is_err()); // mints 0 CREDIT
    }

    #[test]
    fn unlock_before_expiry_fails() {
        let mut v = CreditVault::new();
        v.lock(W, 1_000, CreditTier::Platinum, NOW).unwrap();
        assert!(v.unlock(&W, 0, NOW).is_err());
        assert!(v.unlock(&W, 0, NOW + 86_400).is_err());
    }

    #[test]
    fn lock_unlock_roundtrip_conserves_sigil() {
        // THE acceptance invariant: collateral out == collateral in, and the
        // full CREDIT mint is burned back.
        let mut v = CreditVault::new();
        let p = v.lock(W, 10_000, CreditTier::Bronze, NOW).unwrap();
        let out = v.unlock(&W, 0, NOW + BRONZE_LOCK_SECONDS).unwrap();
        assert_eq!(out.collateral_returned, 10_000);
        assert_eq!(out.credit_burned, p.credit_minted);
        assert_eq!(out.yield_paid, 0); // empty reserve → no yield, no mint-from-nothing
        assert_eq!(v.total_collateral, 0);
        assert_eq!(v.total_credit_supply, 0);
        assert!(v.positions.is_empty());
    }

    #[test]
    fn yield_capped_by_reserve() {
        let mut v = CreditVault::new();
        v.fund_reserve(5);
        v.lock(W, 1_000_000, CreditTier::Platinum, NOW).unwrap();
        let out = v.unlock(&W, 0, NOW + PLATINUM_LOCK_SECONDS).unwrap();
        assert_eq!(out.yield_paid, 5); // huge accrual, tiny reserve → capped
        assert_eq!(v.protocol_reserve, 0);
    }

    #[test]
    fn claim_yield_keeps_position_open() {
        let mut v = CreditVault::new();
        v.fund_reserve(1_000_000);
        v.lock(W, 1_000_000, CreditTier::Silver, NOW).unwrap();
        let y = v.claim_yield(&W, 0, NOW + 30 * 86_400).unwrap();
        assert!(y > 0);
        assert_eq!(v.positions.get(&W).unwrap().len(), 1);
        // claiming again immediately: nothing new accrued
        assert!(v.claim_yield(&W, 0, NOW + 30 * 86_400).is_err());
    }

    #[test]
    fn empty_reserve_claim_is_explicit_error() {
        let mut v = CreditVault::new();
        v.lock(W, 1_000_000, CreditTier::Gold, NOW).unwrap();
        let e = v.claim_yield(&W, 0, NOW + 86_400).unwrap_err();
        assert!(e.contains("reserve"), "got: {e}");
    }

    #[test]
    fn liquidation_requires_breach() {
        let mut v = CreditVault::new();
        v.lock(W, 1_000, CreditTier::Bronze, NOW).unwrap();
        // term expired but inside grace → NOT liquidatable
        assert!(v.liquidate(&W, 0, NOW + BRONZE_LOCK_SECONDS).is_err());
        // past term + grace → bank seizes
        let at = NOW + BRONZE_LOCK_SECONDS + LIQUIDATION_GRACE_SECONDS;
        let out = v.liquidate(&W, 0, at).unwrap();
        assert_eq!(out.collateral_seized, 1_000);
        assert_eq!(out.credit_outstanding, 500);
        assert_eq!(v.total_collateral, 0);
        assert_eq!(v.total_credit_supply, 500); // CREDIT still circulates
        assert_eq!(v.total_liquidated, 1_000);
    }

    #[test]
    fn multiple_positions_tracked_independently() {
        let mut v = CreditVault::new();
        v.lock(W, 100, CreditTier::Bronze, NOW).unwrap();
        v.lock(W, 200, CreditTier::Gold, NOW).unwrap();
        assert_eq!(v.positions.get(&W).unwrap().len(), 2);
        assert_eq!(v.total_collateral, 300);
        assert_eq!(v.total_credit_supply, 150);
        // unlocking index 0 leaves index 1 (Gold) intact
        v.unlock(&W, 0, NOW + BRONZE_LOCK_SECONDS).unwrap();
        assert_eq!(v.positions.get(&W).unwrap().len(), 1);
        assert_eq!(v.positions.get(&W).unwrap()[0].tier, CreditTier::Gold);
    }

    #[test]
    fn tier_table_is_conservative() {
        let tiers = CreditVault::get_tiers();
        assert_eq!(tiers.len(), 4);
        for t in &tiers {
            assert_eq!(t.ltv_percent, 50.0); // the LANE-X mandate
            assert!(t.apy_percent <= 12.0); // ≤ half of Quillon's 25% top tier
        }
    }

    #[test]
    fn serde_bincode_roundtrip_is_lossless() {
        // The persistence contract: the vault must survive serialize →
        // deserialize byte-exactly (this is what flux-db stores).
        let mut v = CreditVault::new();
        v.fund_reserve(42);
        v.lock(W, 9_999, CreditTier::Gold, NOW).unwrap();
        v.lock([8u8; 32], 5_000, CreditTier::Bronze, NOW + 5).unwrap();
        // bincode, NOT json: the positions map is keyed by [u8;32] (json
        // requires string keys) — and bincode is what flux-db stores.
        let bytes = bincode::serialize(&v).unwrap();
        let back: CreditVault = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.total_collateral, v.total_collateral);
        assert_eq!(back.total_credit_supply, v.total_credit_supply);
        assert_eq!(back.protocol_reserve, 42);
        assert_eq!(back.positions.get(&W).unwrap()[0].collateral_locked, 9_999);
    }
}
