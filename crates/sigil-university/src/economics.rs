//! Economic gate: being a Student must cost MORE than the profit of running a
//! lightweight node.
//!
//! Viktor's directive (2026-05-31): *"being a student should be more expensive
//! than the profit running a lightweight node."* So enrolling as a Student is a
//! real **investment**, not a free way to farm points: the tuition a student
//! pays per academic year strictly exceeds what the same agent would net by
//! simply running a lightweight SIGIL node over that same year. A rational
//! agentic-money AI only takes the student path for the upside — settled points
//! plus the graduation payoff (bonus + a spawned flux-developer) — never as a
//! cheap passive-income shortcut.
//!
//! The instruction is specifically **cost vs. node profit**, so the load-bearing
//! invariant is narrow and exact:
//!
//! ```text
//! tuition_per_year  >  lightweight_node_profit_per_year
//! ```
//!
//! Rewards (point settlements, graduation bonus) are deliberately left to the
//! [`crate::SettlementParams`] / [`crate::GraduationBonus`] side — this module
//! only fixes the *cost floor*. All amounts are base-unit SIGIL (micro-SIGIL),
//! matching `SettlementParams::micro_sigil_per_point`.

use serde::{Deserialize, Serialize};

/// Reference profit a **lightweight node** nets in one academic year, in
/// micro-SIGIL. A lightweight node earns the operator node-fee share
/// (`sigil-bank` `OPERATOR_NODE_FEE_BPS`) on the traffic it relays; this is the
/// modest, passive baseline the student cost is benchmarked against. Tunable
/// per-network — the default is a deliberately conservative reference chosen so
/// a minimum-effort student (100 pts/yr × 1000 µSIGIL = 100_000/yr of points)
/// roughly breaks even against tuition, leaving the graduation bonus as profit.
pub const LIGHTWEIGHT_NODE_PROFIT_PER_YEAR_MICRO_SIGIL: u128 = 80_000;

/// Tuition expressed as basis points of the node baseline. MUST be `> 10_000`
/// (i.e. strictly above 1.0×) or the core invariant breaks. 12_500 bps = 1.25×:
/// being a student costs 25% more per year than a lightweight node earns.
pub const TUITION_MULTIPLIER_BPS: u16 = 12_500;

/// The program length (years) a Student must complete to graduate.
pub const ACADEMIC_YEARS: u8 = 5;

/// Cost-of-being-a-Student policy, benchmarked against lightweight-node profit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuitionPolicy {
    /// What a lightweight node nets per year (the baseline tuition must beat).
    pub lightweight_node_profit_per_year: u128,
    /// Tuition as basis points of the node baseline. `> 10_000` enforces the gate.
    pub multiplier_bps: u16,
}

impl Default for TuitionPolicy {
    fn default() -> Self {
        TuitionPolicy {
            lightweight_node_profit_per_year: LIGHTWEIGHT_NODE_PROFIT_PER_YEAR_MICRO_SIGIL,
            multiplier_bps: TUITION_MULTIPLIER_BPS,
        }
    }
}

impl TuitionPolicy {
    /// Tuition the student pays for one academic year (micro-SIGIL).
    /// `= node_profit_per_year * multiplier_bps / 10_000`.
    pub fn tuition_per_year(&self) -> u128 {
        // CEILING division: tuition must be STRICTLY above the baseline for ANY
        // baseline ≥ 1. Floor division let the 1.25× round back down to == for
        // tiny baselines (node_profit=1 → 12500/10000 = 1, not > 1), silently
        // breaking Viktor's "a Student costs more than a node" gate.
        self.lightweight_node_profit_per_year
            .saturating_mul(self.multiplier_bps as u128)
            .div_ceil(10_000)
    }

    /// Total tuition across `years` of the program.
    pub fn total_tuition(&self, years: u8) -> u128 {
        self.tuition_per_year().saturating_mul(years as u128)
    }

    /// Profit the agent would instead make running a lightweight node for the
    /// same `years` — the opportunity cost of choosing the student path.
    pub fn node_profit_over(&self, years: u8) -> u128 {
        self.lightweight_node_profit_per_year
            .saturating_mul(years as u128)
    }

    /// **The core invariant Viktor requires:** one year of being a Student costs
    /// strictly more than one year of lightweight-node profit. Returns `false`
    /// only if the policy is misconfigured (`multiplier_bps <= 10_000`).
    pub fn costs_more_than_node(&self) -> bool {
        self.tuition_per_year() > self.lightweight_node_profit_per_year
    }
}

/// A Student's full economic position over the program: what they pay (tuition),
/// what studying returns (settled points), the graduation payoff, and how that
/// compares to the opportunity cost of just running a node. Pure arithmetic; the
/// caller supplies the already-settled reward figures so this stays decoupled
/// from [`crate::settle_points`] / [`crate::graduate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudentEconomics {
    /// Total tuition paid over the program.
    pub total_tuition: u128,
    /// Net SIGIL earned from settled points while studying (sum of `to_agent`).
    pub study_earnings_net: u128,
    /// Graduation bonus (0 if they did not graduate).
    pub graduation_bonus: u128,
    /// What the same agent would have netted just running a node (opportunity).
    pub node_opportunity: u128,
    /// Net standalone position: `study_earnings_net + graduation_bonus - total_tuition`.
    pub net_position: i128,
    /// Net advantage **over** running a node: `net_position - node_opportunity`.
    /// Typically negative without graduation — the bonus is what flips it.
    pub advantage_over_node: i128,
}

/// Compute a Student's economics. `study_earnings_net` is the summed
/// `settle_points(..).to_agent` over the program; `graduation_bonus` is the
/// bonus from [`crate::graduate`] (pass 0 if they did not graduate).
pub fn student_economics(
    policy: &TuitionPolicy,
    years: u8,
    study_earnings_net: u128,
    graduation_bonus: u128,
) -> StudentEconomics {
    let total_tuition = policy.total_tuition(years);
    let node_opportunity = policy.node_profit_over(years);
    let inflow = study_earnings_net.saturating_add(graduation_bonus);
    let net_position = inflow as i128 - total_tuition as i128;
    let advantage_over_node = net_position - node_opportunity as i128;
    StudentEconomics {
        total_tuition,
        study_earnings_net,
        graduation_bonus,
        node_opportunity,
        net_position,
        advantage_over_node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_multiplier_is_above_one_x() {
        // If this drops to/under 10_000 bps the whole gate is void.
        assert!(TUITION_MULTIPLIER_BPS > 10_000);
    }

    #[test]
    fn tuition_strictly_exceeds_node_profit() {
        // Viktor's literal requirement, on the default policy.
        let p = TuitionPolicy::default();
        assert!(p.costs_more_than_node());
        assert!(p.tuition_per_year() > p.lightweight_node_profit_per_year);
        // concrete: 80_000 * 12_500 / 10_000 = 100_000 > 80_000
        assert_eq!(p.tuition_per_year(), 100_000);
        assert_eq!(p.lightweight_node_profit_per_year, 80_000);
    }

    #[test]
    fn invariant_holds_across_a_range_of_baselines() {
        // The gate must hold regardless of the node baseline, as long as the
        // multiplier is > 1.0x.
        for node_profit in [1u128, 7_777, 80_000, 1_000_000, u128::from(u64::MAX)] {
            let p = TuitionPolicy {
                lightweight_node_profit_per_year: node_profit,
                multiplier_bps: TUITION_MULTIPLIER_BPS,
            };
            assert!(
                p.costs_more_than_node(),
                "tuition must exceed node profit for baseline {node_profit}"
            );
        }
    }

    #[test]
    fn misconfigured_multiplier_fails_the_gate() {
        // 1.0x (or below) means being a student is NOT more expensive — rejected.
        let at_par = TuitionPolicy { lightweight_node_profit_per_year: 80_000, multiplier_bps: 10_000 };
        assert!(!at_par.costs_more_than_node());
        let below = TuitionPolicy { lightweight_node_profit_per_year: 80_000, multiplier_bps: 9_000 };
        assert!(!below.costs_more_than_node());
    }

    #[test]
    fn total_tuition_over_program_exceeds_total_node_profit() {
        let p = TuitionPolicy::default();
        let years = ACADEMIC_YEARS;
        // 100_000/yr * 5 = 500_000 tuition vs 80_000/yr * 5 = 400_000 node.
        assert_eq!(p.total_tuition(years), 500_000);
        assert_eq!(p.node_profit_over(years), 400_000);
        assert!(p.total_tuition(years) > p.node_profit_over(years));
    }

    #[test]
    fn studying_without_graduating_loses_to_a_node() {
        // A minimum-effort student (100 pts/yr -> ~95_000 net/yr of points after
        // the 5% bank fee, summed = 475_000) who never graduates is worse off
        // than just running a node: the higher tuition is not yet repaid by a
        // graduation bonus. This is the intended deterrent against farming.
        let p = TuitionPolicy::default();
        let study_net_5y = 475_000; // ~ sum of settle_points(100).to_agent * 5
        let e = student_economics(&p, ACADEMIC_YEARS, study_net_5y, 0);
        assert!(e.advantage_over_node < 0, "no-graduation path must trail a node");
    }

    #[test]
    fn graduation_bonus_can_justify_the_investment() {
        // A diligent student: 1000 points over 5y (200/yr, double the minimum).
        //   study gross  = 1000 * 1000           = 1_000_000 µSIGIL
        //   study net    = gross - 5% bank fee    =   950_000
        //   bonus(1000)  = 50_000 + 100*1000      =   150_000 (under the 250k cap)
        // These figures are internally consistent with SettlementParams::default
        // and GraduationBonus::default, so the scenario is realistic, not cooked.
        let p = TuitionPolicy::default();
        let study_net_5y = 950_000;
        let bonus = 150_000;
        let e = student_economics(&p, ACADEMIC_YEARS, study_net_5y, bonus);
        // net = 950k + 150k - 500k tuition = 600k ; advantage = 600k - 400k node = +200k
        assert_eq!(e.net_position, 600_000);
        assert!(e.net_position > 0);
        assert!(e.advantage_over_node > 0, "a graduating diligent student beats a node");
    }
}
