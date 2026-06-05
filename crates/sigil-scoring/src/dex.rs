// DEX-specific scorers: pools, swaps, LP positions.
//
// Each scorer composes the three SIGIL axes:
//   SAP     — per-subject 5-dim score from `subject::SubjectScoreTable`
//   X-Algo  — per-subject 5-dim cross score from `subject::XAlgoTable`
//   K-param — chain-wide K-correlation from `kparam::KParameterEngine`
//
// Semantic re-interpretation of SAP dimensions for finance subjects:
//
//   SAP dim         Validator (default)     Pool                Wallet            LP position
//   ──────────────────────────────────────────────────────────────────────────────────────────
//   contribution    vertices/round          trade volume rate   tx submit rate    fee-share rate
//   latency         response p50            time-to-fill        time-to-confirm   time-since-deposit
//   stake           QUG staked              liquidity depth     balance vs supply shares vs pool
//   accuracy        no equivocation         IL / price tracking tx success rate   no-rugpull
//   uptime          rounds participated     active-trade epochs active epochs     epochs in pool
//
// Swap quality is the headline metric the user requested: every executed
// swap gets a SwapQualityScore composed from effective-price quality + pool
// health + counterparty trust + chain K-correlation.

use serde::{Deserialize, Serialize};

use crate::kparam::KParameterEngine;
use crate::subject::{SubjectScoreTable, XAlgoTable};
use crate::{composite_score, CompositeWeights, composite_score_weighted};

/// Headline pool health metric. Higher = better trading venue.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct PoolHealthScore {
    /// Composite 0..1.
    pub total: f64,
    /// Liquidity depth normalised against the largest pool.
    pub liquidity: f64,
    /// Trades-per-unit-time normalised.
    pub volume_velocity: f64,
    /// Recent price standard deviation as an inverse score (more stable → higher).
    pub price_stability: f64,
    /// Fraction of recent epochs with at least one swap.
    pub uptime: f64,
}

/// Per-swap execution quality score (the user's headline ask).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct SwapQualityScore {
    /// Composite 0..1 — what the wallet UI displays as the swap-grade badge.
    pub total: f64,
    /// 1.0 - slippage_bps/10000; 0 if slippage > 100%.
    pub effective_price: f64,
    /// PoolHealthScore.total at swap-time.
    pub pool_health: f64,
    /// Counterparty trust (sender's composite from validator/wallet tables).
    pub counterparty_trust: f64,
    /// Chain K-correlation snapshot at swap-time.
    pub k_correlation: f64,
}

/// LP position quality / reputation.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct LpReputationScore {
    pub total: f64,
    /// Fraction of accrued fees vs deployed capital (clamped to 1.0).
    pub fees_per_deploy: f64,
    /// Tenure: epochs in pool / pool epochs since creation.
    pub tenure: f64,
    /// Has the LP withdrawn during anomalous pool epochs? 1.0 = never.
    pub stability: f64,
}

/// Inputs needed to score a single swap.
#[derive(Clone, Debug)]
pub struct SwapInput<PoolId, AccountId> {
    pub pool: PoolId,
    pub sender: AccountId,
    /// Best-execution reference price (e.g. 30-block VWAP).
    pub reference_price: f64,
    /// Actual price the swap got filled at.
    pub effective_price: f64,
    /// Slippage in basis points (1 bp = 0.01%). Optional if effective/reference
    /// are provided — set to None to derive.
    pub slippage_bps: Option<u16>,
}

/// Compute a pool's health score using the same SAP machinery applied to a
/// pool-keyed table. Caller is responsible for keeping the table fresh with
/// up-to-date dimension values (typically updated each block by the indexer).
pub fn score_pool_health<PoolId>(
    pool: &PoolId,
    sap_table: &SubjectScoreTable<PoolId>,
) -> PoolHealthScore
where
    PoolId: std::hash::Hash + Eq + Clone,
{
    let entry = match sap_table.get_full(pool) {
        Some(s) => s,
        None => return PoolHealthScore::default(),
    };
    let c = &entry.components;
    PoolHealthScore {
        total: entry.total,
        liquidity: c.stake,
        volume_velocity: c.contribution,
        // Re-interpret accuracy (price-tracking proxy) AND uptime to derive
        // a stability axis. Half-and-half weighting.
        price_stability: (c.accuracy + c.uptime) / 2.0,
        uptime: c.uptime,
    }
}

/// Compute LP reputation from a wallet/LP-keyed SAP table.
pub fn score_lp_reputation<LpId>(
    lp: &LpId,
    sap_table: &SubjectScoreTable<LpId>,
) -> LpReputationScore
where
    LpId: std::hash::Hash + Eq + Clone,
{
    let entry = match sap_table.get_full(lp) {
        Some(s) => s,
        None => return LpReputationScore::default(),
    };
    let c = &entry.components;
    LpReputationScore {
        total: entry.total,
        fees_per_deploy: c.contribution,
        tenure: c.uptime,
        stability: c.accuracy,
    }
}

/// Compute a swap-quality score end-to-end.
///
/// Inputs:
/// * `swap`             — the swap details (pool, sender, prices).
/// * `pool_sap`         — SAP table keyed on pool id (must be pre-updated).
/// * `wallet_sap`       — SAP table keyed on account id (sender's trust).
/// * `wallet_xalgo`     — X-Algo table for the sender (provides historical
///                        consensus / non-spam history).
/// * `kparam`           — chain K-parameter engine.
/// * `weights`          — composite weights (`None` uses default 0.5/0.3/0.2).
///
/// Returns a `SwapQualityScore` with both the composite `.total` and the
/// breakdown so wallet UIs can render the "why" tooltip.
pub fn score_swap<PoolId, AccountId>(
    swap: &SwapInput<PoolId, AccountId>,
    pool_sap: &SubjectScoreTable<PoolId>,
    wallet_sap: &SubjectScoreTable<AccountId>,
    wallet_xalgo: &XAlgoTable<AccountId>,
    kparam: &KParameterEngine,
    weights: Option<CompositeWeights>,
) -> SwapQualityScore
where
    PoolId: std::hash::Hash + Eq + Clone,
    AccountId: std::hash::Hash + Eq + Clone,
{
    // 1) Effective-price score from slippage (in basis points).
    let slippage_bps = swap.slippage_bps.unwrap_or_else(|| {
        if swap.reference_price <= 0.0 { return 0; }
        let raw = ((swap.effective_price - swap.reference_price).abs() / swap.reference_price) * 10_000.0;
        raw.clamp(0.0, 10_000.0) as u16
    });
    let effective_price = 1.0 - (slippage_bps as f64 / 10_000.0);
    let effective_price = effective_price.clamp(0.0, 1.0);

    // 2) Pool health from the pool-keyed SAP table.
    let pool_health = score_pool_health(&swap.pool, pool_sap).total;

    // 3) Counterparty trust: 0.5·SAP + 0.3·X-Algo of the sender, plus the
    //    swap's effective-price score as the third "axis-substitute" before
    //    composite_score adds the chain K-correlation.
    let sender_sap = wallet_sap.get(&swap.sender).unwrap_or(0.0);
    let sender_xalgo = wallet_xalgo.get(&swap.sender).map(|s| s.total).unwrap_or(0.0);
    let counterparty_trust =
        (sender_sap * 0.6 + sender_xalgo * 0.4).clamp(0.0, 1.0);

    // 4) Chain K-correlation at swap-time.
    let k_correlation = kparam.normalised_correlation();

    // 5) Composite: SAP axis ← average of pool_health and counterparty_trust
    //    (the two SAP-derived inputs), X-Algo axis ← effective_price (execution
    //    quality is intrinsically cross-algorithmic — price vs reference),
    //    K-axis ← k_correlation.
    let sap_axis = (pool_health + counterparty_trust) / 2.0;
    let xalgo_axis = effective_price;
    let kparam_axis = k_correlation;

    let total = match weights {
        None => composite_score(sap_axis, xalgo_axis, kparam_axis),
        Some(w) => composite_score_weighted(sap_axis, xalgo_axis, kparam_axis, w),
    };

    SwapQualityScore {
        total,
        effective_price,
        pool_health,
        counterparty_trust,
        k_correlation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::{SapComponents, XAlgoTable};

    type PoolId = [u8; 8];
    type AccountId = [u8; 8];

    fn alice() -> AccountId { *b"alice000" }
    fn pool_a() -> PoolId { *b"poolXYZ0" }

    fn fresh_pool_sap() -> SubjectScoreTable<PoolId> {
        let mut t: SubjectScoreTable<PoolId> = SubjectScoreTable::new();
        t.update(pool_a(), SapComponents {
            contribution: 0.85, // healthy volume
            latency: 0.90,
            stake: 0.80,        // deep liquidity
            accuracy: 0.95,
            uptime: 0.90,
        });
        t
    }

    fn fresh_wallet_sap() -> SubjectScoreTable<AccountId> {
        let mut t: SubjectScoreTable<AccountId> = SubjectScoreTable::new();
        t.update(alice(), SapComponents {
            contribution: 0.7,
            latency: 0.8,
            stake: 0.5,
            accuracy: 1.0,
            uptime: 0.9,
        });
        t
    }

    fn fresh_wallet_xalgo() -> XAlgoTable<AccountId> {
        let mut x: XAlgoTable<AccountId> = XAlgoTable::new();
        for r in 0u64..30 {
            x.record_round(alice(), r, true, 0.95);
        }
        x
    }

    #[test]
    fn pool_health_basic() {
        let t = fresh_pool_sap();
        let h = score_pool_health(&pool_a(), &t);
        assert!(h.total > 0.0);
        assert!((h.liquidity - 0.80).abs() < 1e-9);
        assert!((h.volume_velocity - 0.85).abs() < 1e-9);
    }

    #[test]
    fn pool_health_unknown_pool_zero() {
        let t = fresh_pool_sap();
        let unknown: PoolId = *b"nopool00";
        let h = score_pool_health(&unknown, &t);
        assert_eq!(h.total, 0.0);
        assert_eq!(h.liquidity, 0.0);
    }

    #[test]
    fn swap_quality_zero_slippage_best_score() {
        let pool_sap = fresh_pool_sap();
        let wallet_sap = fresh_wallet_sap();
        let wallet_xalgo = fresh_wallet_xalgo();
        let mut k = KParameterEngine::new(0.7).with_noise_amplitude(0.0);
        k.update_event(15.0, "SwapExecuted");

        let swap = SwapInput {
            pool: pool_a(),
            sender: alice(),
            reference_price: 100.0,
            effective_price: 100.0, // zero slippage
            slippage_bps: None,
        };
        let s = score_swap(&swap, &pool_sap, &wallet_sap, &wallet_xalgo, &k, None);
        assert!((s.effective_price - 1.0).abs() < 1e-9, "effective_price should be 1.0 at zero slippage");
        assert!(s.pool_health > 0.0);
        assert!(s.counterparty_trust > 0.0);
        assert!(s.total > 0.5, "healthy swap should score >0.5, got {}", s.total);
    }

    #[test]
    fn swap_quality_heavy_slippage_low_score() {
        let pool_sap = fresh_pool_sap();
        let wallet_sap = fresh_wallet_sap();
        let wallet_xalgo = fresh_wallet_xalgo();
        let mut k = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        k.update_event(15.0, "SwapExecuted");

        let swap = SwapInput {
            pool: pool_a(),
            sender: alice(),
            reference_price: 100.0,
            effective_price: 50.0, // 50% slippage
            slippage_bps: None,
        };
        let s = score_swap(&swap, &pool_sap, &wallet_sap, &wallet_xalgo, &k, None);
        // 50% slippage = 5000 bps → effective_price = 1 - 0.5 = 0.5
        assert!((s.effective_price - 0.5).abs() < 0.01);
        // Composite should be meaningfully lower than the zero-slippage case.
        assert!(s.total < 0.7);
    }

    #[test]
    fn swap_quality_explicit_slippage_bps_overrides() {
        let pool_sap = fresh_pool_sap();
        let wallet_sap = fresh_wallet_sap();
        let wallet_xalgo = fresh_wallet_xalgo();
        let mut k = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        k.update_event(10.0, "SwapExecuted");

        let swap = SwapInput {
            pool: pool_a(),
            sender: alice(),
            reference_price: 100.0,
            effective_price: 99.0,        // would derive to 100 bps
            slippage_bps: Some(500),      // but caller asserted 500
        };
        let s = score_swap(&swap, &pool_sap, &wallet_sap, &wallet_xalgo, &k, None);
        // 500 bps → effective_price = 1 - 0.05 = 0.95
        assert!((s.effective_price - 0.95).abs() < 1e-9);
    }

    #[test]
    fn lp_reputation_basic() {
        let mut t: SubjectScoreTable<&'static str> = SubjectScoreTable::new();
        t.update("lp-alice-pool-abc", SapComponents {
            contribution: 0.6,
            latency: 0.0,
            stake: 0.4,
            accuracy: 1.0,
            uptime: 0.85,
        });
        let r = score_lp_reputation(&"lp-alice-pool-abc", &t);
        assert!((r.fees_per_deploy - 0.6).abs() < 1e-9);
        assert!((r.tenure - 0.85).abs() < 1e-9);
        assert!((r.stability - 1.0).abs() < 1e-9);
    }

    #[test]
    fn swap_quality_unknown_sender_still_works() {
        let pool_sap = fresh_pool_sap();
        let wallet_sap: SubjectScoreTable<AccountId> = SubjectScoreTable::new(); // empty
        let wallet_xalgo: XAlgoTable<AccountId> = XAlgoTable::new();             // empty
        let mut k = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        k.update_event(10.0, "SwapExecuted");

        let swap = SwapInput {
            pool: pool_a(),
            sender: alice(),
            reference_price: 100.0,
            effective_price: 100.0,
            slippage_bps: None,
        };
        let s = score_swap(&swap, &pool_sap, &wallet_sap, &wallet_xalgo, &k, None);
        // Unknown sender → counterparty_trust = 0; but pool + effective_price + K still contribute.
        assert_eq!(s.counterparty_trust, 0.0);
        assert!(s.total > 0.0);
    }
}
