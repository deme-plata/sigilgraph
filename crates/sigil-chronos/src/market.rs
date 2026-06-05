//! market.rs — the agentic-money DEX market, simulated + scored.
//!
//! Thousands of AI trading agents, each running a strategy, swap on a real
//! `sigil-dex` constant-product pool. Millions of trades, billions of volume
//! (sim units), deterministic. Then every agent's trading is scored by two
//! Flux scorers *repurposed for trade quality*, and the BEST strategy for DEX
//! swaps is reported — for CHIRON to render, and for the agent economy to
//! discover its own optimal behavior.
//!
//!   X-Algo score — predictive trade QUALITY (profit · win-rate · consistency ·
//!                  capital efficiency), the trade-side analogue of the build
//!                  X-Algo's 8-dimension forecast.
//!   SAP score    — trade EFFICIENCY (velocity · capital-utilization · fee
//!                  efficiency), the trade-side analogue of compile-velocity /
//!                  cache-health / swarm-utilization.
//!
//! Honest: the scores reuse the *dimensions* of Flux's X-Algo/SAP scorers, not
//! their literal build-trained models — they're heuristic trade scores in the
//! same shape, 0–100.

use std::time::Instant;

use sigil_dex::{swap, Pool, SwapDirection};

/// Trading strategies the agentic-money AIs run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Random direction + size.
    Random,
    /// Buy the rising side (trend-follow).
    Momentum,
    /// Buy the cheap side (revert to the moving average).
    MeanRevert,
    /// Trade toward the anchor (fair) price.
    Arb,
    /// Occasional large trades.
    Whale,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Strategy::Random => "random",
            Strategy::Momentum => "momentum",
            Strategy::MeanRevert => "mean-revert",
            Strategy::Arb => "arbitrage",
            Strategy::Whale => "whale",
        }
    }
    const ALL: [Strategy; 5] = [
        Strategy::Random, Strategy::Momentum, Strategy::MeanRevert, Strategy::Arb, Strategy::Whale,
    ];
}

/// splitmix64 — deterministic, dep-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pct(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Agent {
    strat: Strategy,
    a: u128,
    b: u128,
    a0: u128,
    b0: u128,
    trades: u64,
    volume_a: u128, // volume measured in token-A units (BtoA legs converted)
    fees_paid: u128,
}

/// Per-strategy scoreboard.
#[derive(Clone, Debug)]
pub struct StrategyScore {
    pub strategy: &'static str,
    pub agents: u64,
    pub avg_pnl_pct: f64,
    pub xalgo: f64,
    pub sap: f64,
}

/// The market run result.
#[derive(Clone, Debug)]
pub struct MarketReport {
    pub agents: u64,
    pub trades: u64,
    pub volume_a: u128,
    pub start_price: f64,
    pub final_price: f64,
    pub elapsed_ms: u128,
    pub trades_per_sec: f64,
    pub best_strategy: &'static str,
    pub best_xalgo: f64,
    pub best_sap: f64,
    pub per_strategy: Vec<StrategyScore>,
}

impl MarketReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "🏦 {} agents · {} trades · {:.3e} volume(A) · {:.0} trades/s · {}ms\n   price {:.4} → {:.4}\n   ── strategy scoreboard (X-Algo = quality, SAP = efficiency) ──\n",
            self.agents, self.trades, self.volume_a as f64, self.trades_per_sec, self.elapsed_ms,
            self.start_price, self.final_price
        );
        for st in &self.per_strategy {
            s.push_str(&format!(
                "   {:<12} pnl {:+6.2}%  X-Algo {:5.1}  SAP {:5.1}  ({} agents)\n",
                st.strategy, st.avg_pnl_pct, st.xalgo, st.sap, st.agents
            ));
        }
        s.push_str(&format!(
            "   ★ BEST strategy: {} (X-Algo {:.1}, SAP {:.1})",
            self.best_strategy, self.best_xalgo, self.best_sap
        ));
        s
    }
}

fn price(p: &Pool) -> f64 {
    if p.reserve_a == 0 { return 0.0; }
    p.reserve_b as f64 / p.reserve_a as f64
}

/// Run the market: `n_agents` trade for `n_ticks` rounds on one pool.
/// Deterministic in `seed`. Returns the scored report.
pub fn run_market(n_agents: u64, n_ticks: u64, seed: u64) -> MarketReport {
    let n_agents = n_agents.max(5);
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);

    // Deep pool so millions of small trades don't drain it.
    let mut pool = Pool {
        reserve_a: 1_000_000_000_000u128,
        reserve_b: 1_000_000_000_000u128,
        total_shares: 1_000_000_000_000u128,
        fee_bps: 30,
        accrued_fees_a: 0,
        accrued_fees_b: 0,
    };
    let start_price = price(&pool);

    // Agents: balances, strategy round-robin.
    let init_a = 10_000_000u128;
    let init_b = 10_000_000u128;
    let mut agents: Vec<Agent> = (0..n_agents)
        .map(|i| Agent {
            strat: Strategy::ALL[(i as usize) % 5],
            a: init_a, b: init_b, a0: init_a, b0: init_b,
            trades: 0, volume_a: 0, fees_paid: 0,
        })
        .collect();

    // Price history for momentum/mean-revert (short SMA).
    let mut sma = start_price;
    let mut last_price = start_price;
    let anchor = start_price; // arb target

    let mut trades = 0u64;
    let mut volume_a = 0u128;
    let t0 = Instant::now();

    for _tick in 0..n_ticks {
        let cur = price(&pool);
        sma = sma * 0.95 + cur * 0.05;
        let rising = cur >= last_price;
        last_price = cur;

        for ag in agents.iter_mut() {
            // Strategy → (direction, amount-fraction-of-balance).
            let (dir, frac, want) = decide(ag.strat, cur, sma, anchor, rising, &mut rng);
            if !want { continue; }
            let (bal, amount) = match dir {
                SwapDirection::AtoB => (ag.a, ((ag.a as f64) * frac) as u128),
                SwapDirection::BtoA => (ag.b, ((ag.b as f64) * frac) as u128),
            };
            if amount == 0 || amount > bal { continue; }
            match swap(&pool, dir, amount, 0) {
                Ok(out) => {
                    pool = out.pool_after;
                    match dir {
                        SwapDirection::AtoB => { ag.a -= amount; ag.b += out.amount_out; ag.volume_a += amount; }
                        SwapDirection::BtoA => { ag.b -= amount; ag.a += out.amount_out;
                            // convert the B-leg volume to A at current price for a common unit
                            ag.volume_a += (amount as f64 / cur.max(1e-9)) as u128; }
                    }
                    ag.fees_paid += out.fee_amount;
                    ag.trades += 1;
                    trades += 1;
                    volume_a += amount;
                }
                Err(_) => { /* slippage/zero/overflow guard — skip */ }
            }
        }
    }

    let elapsed_ms = t0.elapsed().as_millis();
    let final_price = price(&pool);

    // ── score every agent, aggregate per strategy ──
    let mut acc: std::collections::HashMap<&'static str, (u64, f64, f64, f64)> = Default::default();
    for ag in &agents {
        // mark-to-market in token-A units at the final price.
        let v0 = ag.a0 as f64 + ag.b0 as f64 / start_price.max(1e-9);
        let v1 = ag.a as f64 + ag.b as f64 / final_price.max(1e-9);
        let pnl_pct = if v0 > 0.0 { (v1 - v0) / v0 * 100.0 } else { 0.0 };

        // X-Algo (quality): profit + consistency(activity) + capital efficiency.
        let profit = (pnl_pct.clamp(-50.0, 50.0) + 50.0); // 0..100
        let cap_eff = (ag.volume_a as f64 / (v0 + 1.0)).min(50.0) * 1.0; // turnover
        let xalgo = (0.7 * profit + 0.3 * (50.0 + cap_eff)).clamp(0.0, 100.0);

        // SAP (efficiency): trade velocity + low fee drag + capital utilization.
        let velocity = (ag.trades as f64 / n_ticks as f64 * 100.0).min(100.0);
        let fee_drag = (ag.fees_paid as f64 / (ag.volume_a as f64 + 1.0)) * 10_000.0; // bps
        let fee_eff = (100.0 - fee_drag.min(100.0)).max(0.0);
        let sap = (0.5 * velocity + 0.5 * fee_eff).clamp(0.0, 100.0);

        let e = acc.entry(ag.strat.name()).or_insert((0, 0.0, 0.0, 0.0));
        e.0 += 1; e.1 += pnl_pct; e.2 += xalgo; e.3 += sap;
    }

    let mut per_strategy: Vec<StrategyScore> = acc
        .into_iter()
        .map(|(strategy, (n, pnl, xa, sa))| StrategyScore {
            strategy, agents: n,
            avg_pnl_pct: pnl / n as f64,
            xalgo: xa / n as f64,
            sap: sa / n as f64,
        })
        .collect();
    per_strategy.sort_by(|a, b| b.xalgo.partial_cmp(&a.xalgo).unwrap_or(std::cmp::Ordering::Equal));

    let best = per_strategy.first().cloned().unwrap_or(StrategyScore {
        strategy: "none", agents: 0, avg_pnl_pct: 0.0, xalgo: 0.0, sap: 0.0,
    });
    let secs = (elapsed_ms as f64 / 1000.0).max(1e-6);

    MarketReport {
        agents: n_agents,
        trades,
        volume_a,
        start_price,
        final_price,
        elapsed_ms,
        trades_per_sec: trades as f64 / secs,
        best_strategy: best.strategy,
        best_xalgo: best.xalgo,
        best_sap: best.sap,
        per_strategy,
    }
}

/// Strategy decision → (direction, fraction-of-balance, do-trade?).
fn decide(s: Strategy, cur: f64, sma: f64, anchor: f64, rising: bool, rng: &mut Rng) -> (SwapDirection, f64, bool) {
    let r = rng.pct();
    match s {
        Strategy::Random => {
            let dir = if r < 0.5 { SwapDirection::AtoB } else { SwapDirection::BtoA };
            (dir, 0.001 + rng.pct() * 0.01, rng.pct() < 0.6)
        }
        Strategy::Momentum => {
            // rising price (more B per A) → buy A (BtoA).
            let dir = if rising { SwapDirection::BtoA } else { SwapDirection::AtoB };
            (dir, 0.002 + rng.pct() * 0.008, rng.pct() < 0.5)
        }
        Strategy::MeanRevert => {
            // price above SMA → A is expensive in B → sell A (AtoB).
            let dir = if cur > sma { SwapDirection::AtoB } else { SwapDirection::BtoA };
            (dir, 0.002 + rng.pct() * 0.008, rng.pct() < 0.5)
        }
        Strategy::Arb => {
            // push price back toward anchor.
            let dir = if cur > anchor { SwapDirection::AtoB } else { SwapDirection::BtoA };
            let gap = ((cur - anchor).abs() / anchor.max(1e-9)).min(0.05);
            (dir, 0.002 + gap, gap > 0.0005)
        }
        Strategy::Whale => {
            let dir = if r < 0.5 { SwapDirection::AtoB } else { SwapDirection::BtoA };
            (dir, 0.05 + rng.pct() * 0.10, rng.pct() < 0.05) // rare, large
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_runs_and_scores() {
        let r = run_market(500, 100, 42);
        assert_eq!(r.agents, 500);
        assert!(r.trades > 1000, "should execute many trades, got {}", r.trades);
        assert_eq!(r.per_strategy.len(), 5, "all 5 strategies represented");
        assert!(r.best_xalgo > 0.0);
    }

    #[test]
    fn deterministic() {
        let a = run_market(300, 80, 7);
        let b = run_market(300, 80, 7);
        assert_eq!(a.trades, b.trades);
        assert_eq!(a.best_strategy, b.best_strategy);
    }
}
