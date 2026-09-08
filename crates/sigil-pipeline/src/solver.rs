//! **Route / solver engine** — turns a [`CrossChainIntent`] into an executable plan out of
//! the primitives that actually exist: external ingress, shielded transit, a DEX swap
//! (real constant-product maths, 30 bps), and an EVM egress. A route that cannot clear the
//! user's `min_output` or `deadline` is never returned; an intent no primitive can serve
//! gets `NoRoute`, never a fake leg.

use serde::{Deserialize, Serialize};

use crate::eth_out::DECIMAL_SHIFT;
use crate::external::{AssetId, ExternalChain};
use crate::intent::CrossChainIntent;
use crate::private_mid::Rate;
use crate::PipelineError;

/// A constant-product pool `x·y = k` with a fee in basis points on the input side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub chain: ExternalChain,
    pub a: AssetId,
    pub b: AssetId,
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub fee_bps: u32,
}

impl Pool {
    /// Uniswap-V2 exact-in: `out = (in·(10000−fee)·R_out) / (R_in·10000 + in·(10000−fee))`.
    pub fn quote(&self, from: AssetId, amount_in: u128) -> Option<u128> {
        let (r_in, r_out) = if from == self.a {
            (self.reserve_a, self.reserve_b)
        } else if from == self.b {
            (self.reserve_b, self.reserve_a)
        } else {
            return None;
        };
        if amount_in == 0 || r_in == 0 || r_out == 0 {
            return None;
        }
        let f = 10_000u128 - self.fee_bps as u128;
        let with_fee = amount_in.checked_mul(f)?;
        let num = with_fee.checked_mul(r_out)?;
        let den = r_in.checked_mul(10_000)?.checked_add(with_fee)?;
        Some(num / den)
    }

    pub fn other(&self, from: AssetId) -> Option<AssetId> {
        if from == self.a { Some(self.b) } else if from == self.b { Some(self.a) } else { None }
    }

    /// Spot price of `from` in units of the other asset (scaled 1e18), before fee.
    pub fn spot_1e18(&self, from: AssetId) -> Option<u128> {
        let (r_in, r_out) = if from == self.a { (self.reserve_a, self.reserve_b) } else { (self.reserve_b, self.reserve_a) };
        if r_in == 0 { return None; }
        r_out.checked_mul(1_000_000_000_000_000_000)?.checked_div(r_in)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Leg {
    /// Verified external state in: `amount` of `asset` becomes a hidden note.
    ExternalIngress { source: ExternalChain, asset: AssetId, amount: u64 },
    /// The private middle: `glyphs` cross SIGIL under a STARK to the exit vault.
    ShieldedTransit { glyphs: u64, fee_glyphs: u64 },
    /// The vault's settled exit minted as wrapped SIGIL on the destination chain.
    MintWrapped { chain: ExternalChain, wei: u128 },
    /// A constant-product swap on the destination chain.
    DexSwap { chain: ExternalChain, from: AssetId, to: AssetId, amount_in: u128, amount_out: u128, pool: Pool },
    /// Final delivery to the recipient.
    Deliver { chain: ExternalChain, asset: AssetId, amount: u128, to: [u8; 20] },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub intent_id: [u8; 32],
    pub legs: Vec<Leg>,
    pub amount_out: u128,
    pub asset_out: AssetId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// No composition of existing primitives serves this intent. The reason names the
    /// missing primitive so it can be built, not papered over.
    NoRoute(String),
    BelowMinOutput { quoted: u128, min: u128 },
    Expired { deadline: u64, now: u64 },
    Overflow,
}

impl From<RouteError> for PipelineError {
    fn from(e: RouteError) -> Self {
        PipelineError::Route(format!("{e:?}"))
    }
}

/// Everything the solver may draw on.
#[derive(Debug, Clone)]
pub struct Market {
    pub rate: Rate,
    pub transit_fee_glyphs: u64,
    pub pools: Vec<Pool>,
}

/// Solve an intent against the market at time `now`. Deterministic: same inputs, same plan.
pub fn solve(intent: &CrossChainIntent, market: &Market, now: u64) -> Result<ExecutionPlan, RouteError> {
    if now > intent.deadline {
        return Err(RouteError::Expired { deadline: intent.deadline, now });
    }
    // Ingress: only Bitcoin-family sources have a verifier today.
    match (intent.source, intent.input) {
        (ExternalChain::Bitcoin | ExternalChain::Lightning, AssetId::Btc) => {}
        (s, a) => return Err(RouteError::NoRoute(format!("no external verifier for {a:?} on {s:?}"))),
    }
    // Egress: the only settlement primitive is a wSIGIL mint on an EVM chain.
    if !matches!(intent.destination, ExternalChain::Polygon | ExternalChain::Ethereum) {
        return Err(RouteError::NoRoute(format!("no egress primitive for {:?}", intent.destination)));
    }
    let glyphs = market.rate.glyphs(intent.amount_in).ok_or(RouteError::Overflow)?;
    let transit = glyphs.checked_sub(market.transit_fee_glyphs).ok_or(RouteError::Overflow)?;
    let wei = (transit as u128).checked_mul(DECIMAL_SHIFT).ok_or(RouteError::Overflow)?;

    let mut legs = vec![
        Leg::ExternalIngress { source: intent.source, asset: intent.input, amount: intent.amount_in },
        Leg::ShieldedTransit { glyphs: transit, fee_glyphs: market.transit_fee_glyphs },
        Leg::MintWrapped { chain: intent.destination, wei },
    ];
    let (asset_out, amount_out) = match intent.output {
        AssetId::WSigil => (AssetId::WSigil, wei),
        AssetId::Sigil => return Err(RouteError::NoRoute("native SIGIL cannot be delivered on an EVM chain; ask for wSIGIL".into())),
        want => {
            // One hop: the best pool on the destination chain from wSIGIL to `want`.
            let best = market
                .pools
                .iter()
                .filter(|p| p.chain == intent.destination && p.other(AssetId::WSigil) == Some(want))
                .filter_map(|p| p.quote(AssetId::WSigil, wei).map(|out| (out, *p)))
                .max_by_key(|(out, _)| *out);
            match best {
                Some((out, pool)) => {
                    legs.push(Leg::DexSwap { chain: intent.destination, from: AssetId::WSigil, to: want, amount_in: wei, amount_out: out, pool });
                    (want, out)
                }
                None => return Err(RouteError::NoRoute(format!("no wSIGIL/{want:?} pool on {:?}", intent.destination))),
            }
        }
    };
    if amount_out < intent.min_output {
        return Err(RouteError::BelowMinOutput { quoted: amount_out, min: intent.min_output });
    }
    legs.push(Leg::Deliver { chain: intent.destination, asset: asset_out, amount: amount_out, to: intent.recipient });
    Ok(ExecutionPlan { intent_id: intent.intent_id(), legs, amount_out, asset_out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Privacy;

    /// The live wSIGIL3/USDC pool shape on 2026-09-06: 1 wSIGIL against 1000 USDC-units
    /// (0.001 USDC) — tiny, so slippage is enormous, which is exactly what the solver must surface.
    fn live_like_pool() -> Pool {
        Pool { chain: ExternalChain::Polygon, a: AssetId::WSigil, b: AssetId::Usdc, reserve_a: 1_000_000_000_000_000_000, reserve_b: 1_000, fee_bps: 30 }
    }
    fn deep_pool() -> Pool {
        Pool { chain: ExternalChain::Polygon, a: AssetId::WSigil, b: AssetId::Usdc, reserve_a: 1_000_000 * 10u128.pow(18), reserve_b: 1_000_000 * 10u128.pow(6), fee_bps: 30 }
    }
    fn intent(output: AssetId, min_output: u128) -> CrossChainIntent {
        CrossChainIntent {
            source: ExternalChain::Bitcoin, input: AssetId::Btc, amount_in: 100_000,
            destination: ExternalChain::Polygon, output, min_output,
            recipient: [0xd7; 20], privacy: Privacy::ShieldedTransit, deadline: 2_000_000_000, nonce: 1,
        }
    }
    fn market(pools: Vec<Pool>) -> Market {
        Market { rate: Rate::UNIT, transit_fee_glyphs: 1, pools }
    }

    #[test]
    fn constant_product_quote_matches_uniswap_v2() {
        // 1,000,000 in against 10^6/10^6 reserves at 30 bps: out = 997·1e6·1e6 / (1e6·1e4 + 997·1e6) = 996,006
        let p = Pool { chain: ExternalChain::Polygon, a: AssetId::WSigil, b: AssetId::Usdc, reserve_a: 1_000_000, reserve_b: 1_000_000, fee_bps: 30 };
        // exact: (1e6·9970)·1e6 / (1e6·1e4 + 1e6·9970) = 9.97e15 / 1.997e10 = 499,248.87 → 499,248
        let out = p.quote(AssetId::WSigil, 1_000_000).unwrap();
        assert_eq!(out, 9_970_000_000_000_000 / 19_970_000_000);
        assert_eq!(out, 499_248);
        assert!(out < 500_000, "a 100% pool-size trade must lose to fee + slippage");
        assert_eq!(p.quote(AssetId::Btc, 1), None, "asset not in pool");
        assert_eq!(p.quote(AssetId::WSigil, 0), None);
    }

    #[test]
    fn wsigil_route_needs_no_pool_and_is_exact() {
        let plan = solve(&intent(AssetId::WSigil, 1), &market(vec![]), 1_700_000_000).unwrap();
        // 100,000 sats → 100,000 glyphs − 1 fee = 99,999 glyphs → ×1e8 wei
        assert_eq!(plan.amount_out, 99_999 * DECIMAL_SHIFT);
        assert_eq!(plan.legs.len(), 4);
        assert!(matches!(plan.legs[1], Leg::ShieldedTransit { glyphs: 99_999, fee_glyphs: 1 }));
    }

    #[test]
    fn usdc_route_picks_the_best_pool_and_reports_real_slippage() {
        let m = market(vec![live_like_pool(), deep_pool()]);
        let plan = solve(&intent(AssetId::Usdc, 1), &m, 1_700_000_000).unwrap();
        let swap = plan.legs.iter().find_map(|l| if let Leg::DexSwap { amount_out, pool, .. } = l { Some((*amount_out, *pool)) } else { None }).unwrap();
        assert_eq!(swap.1, deep_pool(), "the deeper pool quotes more");
        // 99,999 glyphs = 0.0000099999 wSIGIL ≈ 9.97 micro-USDC on a 1:1 deep pool (6dp → 9 units)
        assert!(swap.0 >= 9 && swap.0 <= 10, "quoted {}", swap.0);
        assert_eq!(plan.asset_out, AssetId::Usdc);
    }

    #[test]
    fn min_output_and_deadline_are_enforced_and_missing_primitives_are_named() {
        let m = market(vec![deep_pool()]);
        assert!(matches!(solve(&intent(AssetId::Usdc, 1_000_000), &m, 1_700_000_000), Err(RouteError::BelowMinOutput { .. })));
        assert!(matches!(solve(&intent(AssetId::Usdc, 1), &m, 2_000_000_001), Err(RouteError::Expired { .. })));
        assert!(matches!(solve(&intent(AssetId::NativeEvm, 1), &m, 1_700_000_000), Err(RouteError::NoRoute(_))), "no wSIGIL/ETH pool → no route, not a fake leg");
        let mut i = intent(AssetId::WSigil, 1);
        i.source = ExternalChain::Ethereum; i.input = AssetId::NativeEvm;
        assert!(matches!(solve(&i, &m, 1_700_000_000), Err(RouteError::NoRoute(_))));
    }
}
