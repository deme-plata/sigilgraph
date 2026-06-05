//! sigil-fees — SIGIL fee system, Layer 1 (SAP-priced base fee).
//!
//! `fee = BASE × congestion_multiplier × rep_multiplier(sap)`
//!
//! The thesis (Lock #23): SIGIL's DAG targets ~10k blocks/sec, so blockspace
//! is abundant — scarcity is NOT the price signal. Reputation is. A blind fee
//! auction prices scarcity SIGIL doesn't have; SAP-pricing prices behavior,
//! which is the actually-scarce thing (trust). The chain already gossips SAP
//! scores (`flux_p2p::sap::composite_score`), so L1 just reads them.
//!
//! Outcomes:
//!  - elite SAP (≥0.90)  → ~0.05× base — proven contributors barely pay
//!  - trusted (0.70–0.90) → 0.1×–0.3×
//!  - normal (0.40–0.70)  → ~1.0× baseline
//!  - fresh/suspicious (<0.40) → 2×–5× until trust is earned
//!
//! Pure functions: no I/O, no async, no chain state. sigil-tx supplies the
//! SAP score + how full the block is; this answers the cost. Amounts are
//! `u128` base units of native SIGIL (matches sigil-tx's u128 convention).
//!
//! L2 (work-escrow) and L3 (recirculation) are separate modules added in
//! later phases; L1 is the foundation and ships first.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Fee-system parameters. Defaults are sane starting points; a chain can
/// tune them at genesis. All multipliers are basis points (1 bp = 0.01%)
/// so the math stays integer — no float drift in consensus-critical paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeParams {
    /// Base fee in native-SIGIL base units, before any multiplier.
    pub base_fee: u128,
    /// Congestion multiplier applied at 100% block fullness, in bp over 1.0×.
    /// e.g. 10_000 means a full block costs 2× (1.0× + 1.0×) the empty-block fee.
    pub congestion_max_bonus_bps: u32,
    /// rep_multiplier floor (bp) for the highest-reputation agents.
    /// 500 bp = 0.05× — elite SAP pays 5% of base. The high-SAP discount.
    pub rep_floor_bps: u32,
    /// rep_multiplier ceiling (bp) for the lowest-reputation agents.
    /// 50_000 bp = 5× — fresh/abusive pays 5× base.
    pub rep_ceiling_bps: u32,
}

impl Default for FeeParams {
    fn default() -> Self {
        Self {
            base_fee: 1_000,              // 1000 base units = nominal small fee
            congestion_max_bonus_bps: 10_000, // up to +1.0× at a full block
            rep_floor_bps: 500,          // 0.05× for elite SAP (the discount)
            rep_ceiling_bps: 50_000,     // 5× for the worst actors
        }
    }
}

/// One basis-point unit = 1/10_000. A multiplier of 10_000 bp == 1.0×.
const ONE_X_BPS: u128 = 10_000;

/// Reputation → fee multiplier (in basis points).
///
/// SAP score is in [0.0, 1.0] (as `flux_p2p::sap::composite_score` returns).
/// We map it to a multiplier between `rep_ceiling_bps` (at sap=0) and
/// `rep_floor_bps` (at sap=1) on a piecewise curve that rewards high trust
/// steeply and punishes low trust hard:
///
///  - sap ≥ 0.90 → rep_floor (the elite discount, flat)
///  - 0.40 ≤ sap < 0.90 → linear from ~1.0× down to floor
///  - sap < 0.40 → linear from ceiling down to ~1.0×
///
/// Returns basis points; 10_000 == 1.0×.
pub fn rep_multiplier_bps(sap: f64, p: &FeeParams) -> u32 {
    let sap = sap.clamp(0.0, 1.0);

    // Elite band: flat floor.
    if sap >= 0.90 {
        return p.rep_floor_bps;
    }
    // Trusted/normal band [0.40, 0.90): interpolate 1.0× → floor as sap rises.
    if sap >= 0.40 {
        // t = 0 at sap=0.40 (1.0×), t = 1 at sap=0.90 (floor)
        let t = (sap - 0.40) / (0.90 - 0.40);
        let one_x = ONE_X_BPS as f64;
        let floor = p.rep_floor_bps as f64;
        let v = one_x + (floor - one_x) * t;
        return v.round() as u32;
    }
    // Suspicious band [0.0, 0.40): interpolate ceiling → 1.0× as sap rises.
    // t = 0 at sap=0.0 (ceiling), t = 1 at sap=0.40 (1.0×)
    let t = sap / 0.40;
    let ceil = p.rep_ceiling_bps as f64;
    let one_x = ONE_X_BPS as f64;
    let v = ceil + (one_x - ceil) * t;
    v.round() as u32
}

/// Congestion multiplier (in basis points) from block fullness.
///
/// `block_fullness` is in [0.0, 1.0] (0 = empty, 1 = full). At fullness 0 the
/// multiplier is 1.0× (10_000 bp); at fullness 1 it's 1.0× + the configured
/// max bonus. Linear — at SIGIL's throughput congestion is rarely the binding
/// factor, so a gentle linear bump is enough.
pub fn congestion_multiplier_bps(block_fullness: f64, p: &FeeParams) -> u32 {
    let f = block_fullness.clamp(0.0, 1.0);
    (ONE_X_BPS as f64 + p.congestion_max_bonus_bps as f64 * f).round() as u32
}

/// The L1 fee: `base × congestion × rep`, all via basis-point integer math.
///
/// `sap` and `block_fullness` are floats in [0,1]; everything downstream of
/// the multiplier derivation is integer u128 so the actual charged amount is
/// deterministic across nodes (no float in the charged value).
pub fn l1_fee(sap: f64, block_fullness: f64, p: &FeeParams) -> u128 {
    let rep_bps = rep_multiplier_bps(sap, p) as u128;
    let cong_bps = congestion_multiplier_bps(block_fullness, p) as u128;
    // fee = base * (rep/10000) * (cong/10000)
    // Order: multiply first, divide last, to preserve precision. base_fee is
    // small (~1e3) and bps are ≤5e4, so base*rep*cong ≤ ~1e3*5e4*2e4 = 1e12,
    // far inside u128 — no overflow risk at realistic params.
    p.base_fee
        .saturating_mul(rep_bps)
        .saturating_mul(cong_bps)
        / (ONE_X_BPS * ONE_X_BPS)
}

/// Convenience: the fee an *elite* agent pays right now (sap≥0.90), for UIs
/// that want to advertise "minimum possible fee."
pub fn min_possible_fee(block_fullness: f64, p: &FeeParams) -> u128 {
    l1_fee(1.0, block_fullness, p)
}

/// Convenience: the fee a brand-new/zero-rep agent pays (the spam price).
pub fn max_fee(block_fullness: f64, p: &FeeParams) -> u128 {
    l1_fee(0.0, block_fullness, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> FeeParams { FeeParams::default() }

    #[test]
    fn elite_sap_pays_the_floor_discount() {
        // sap ≥ 0.90 → rep_floor (500 bp = 0.05×)
        assert_eq!(rep_multiplier_bps(0.90, &p()), 500);
        assert_eq!(rep_multiplier_bps(0.95, &p()), 500);
        assert_eq!(rep_multiplier_bps(1.00, &p()), 500);
    }

    #[test]
    fn zero_rep_pays_the_ceiling() {
        // sap = 0 → rep_ceiling (50_000 bp = 5×)
        assert_eq!(rep_multiplier_bps(0.0, &p()), 50_000);
    }

    #[test]
    fn normal_sap_is_about_one_x() {
        // sap = 0.40 is the hinge: exactly 1.0× (10_000 bp)
        assert_eq!(rep_multiplier_bps(0.40, &p()), 10_000);
    }

    #[test]
    fn multiplier_is_monotonic_decreasing_in_sap() {
        // Higher reputation must never cost more.
        let pp = p();
        let mut prev = u32::MAX;
        let mut s = 0.0;
        while s <= 1.0 {
            let m = rep_multiplier_bps(s, &pp);
            assert!(m <= prev, "rep_multiplier must be non-increasing in sap; at sap={} got {} > prev {}", s, m, prev);
            prev = m;
            s += 0.05;
        }
    }

    #[test]
    fn congestion_empty_is_one_x_full_is_two_x() {
        let pp = p();
        assert_eq!(congestion_multiplier_bps(0.0, &pp), 10_000); // 1.0×
        assert_eq!(congestion_multiplier_bps(1.0, &pp), 20_000); // 2.0×
        assert_eq!(congestion_multiplier_bps(0.5, &pp), 15_000); // 1.5×
    }

    #[test]
    fn elite_pays_far_less_than_fresh() {
        let pp = p();
        let elite = l1_fee(0.95, 0.0, &pp);
        let fresh = l1_fee(0.0, 0.0, &pp);
        // elite = 1000 * 0.05 = 50 ; fresh = 1000 * 5 = 5000 → 100× gap
        assert_eq!(elite, 50);
        assert_eq!(fresh, 5000);
        assert_eq!(fresh / elite, 100, "fresh should pay 100× what elite pays at empty block");
    }

    #[test]
    fn congestion_compounds_with_reputation() {
        let pp = p();
        // elite at a FULL block: 1000 * 0.05 * 2.0 = 100
        assert_eq!(l1_fee(0.95, 1.0, &pp), 100);
        // fresh at a FULL block: 1000 * 5 * 2.0 = 10_000
        assert_eq!(l1_fee(0.0, 1.0, &pp), 10_000);
    }

    #[test]
    fn min_and_max_helpers_match_l1() {
        let pp = p();
        assert_eq!(min_possible_fee(0.0, &pp), l1_fee(1.0, 0.0, &pp));
        assert_eq!(max_fee(0.0, &pp), l1_fee(0.0, 0.0, &pp));
        assert!(min_possible_fee(0.0, &pp) < max_fee(0.0, &pp));
    }

    #[test]
    fn out_of_range_inputs_are_clamped_not_panicking() {
        let pp = p();
        // negative + >1 sap, negative + >1 fullness — must clamp, never panic
        assert_eq!(rep_multiplier_bps(-5.0, &pp), 50_000); // clamps to 0.0 → ceiling
        assert_eq!(rep_multiplier_bps(9.9, &pp), 500);     // clamps to 1.0 → floor
        let _ = l1_fee(-1.0, 2.0, &pp); // must not panic
        let _ = l1_fee(2.0, -1.0, &pp);
    }

    #[test]
    fn params_serde_roundtrip() {
        let pp = FeeParams { base_fee: 7777, congestion_max_bonus_bps: 12345, rep_floor_bps: 100, rep_ceiling_bps: 99_999 };
        let j = serde_json::to_string(&pp).unwrap();
        let back: FeeParams = serde_json::from_str(&j).unwrap();
        assert_eq!(pp, back);
    }

    #[test]
    fn custom_params_respected() {
        // A chain that wants a steeper elite discount + harsher spam price.
        let pp = FeeParams { base_fee: 10_000, congestion_max_bonus_bps: 0, rep_floor_bps: 100 /*0.01x*/, rep_ceiling_bps: 100_000 /*10x*/ };
        assert_eq!(l1_fee(1.0, 0.0, &pp), 100);    // 10000 * 0.01 = 100
        assert_eq!(l1_fee(0.0, 0.0, &pp), 100_000); // 10000 * 10 = 100000
    }
}
