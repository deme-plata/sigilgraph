//! Protocol constants and calibration knobs, each labelled with what it is.
//!
//! The paper is explicit that several of these are calibration choices with no
//! derivation behind them (the `2*PI`, the `tau = 60s` window, the phase
//! thresholds). They are collected here so nobody has to guess which numbers
//! are physics, which are protocol, and which are "it made the dashboard look
//! right".

/// DAG-Knight tolerance parameter `kappa`. Protocol constant (paper Table 1, P).
pub const KAPPA: f64 = 18.0;

/// Target confirmation depth `tau_confirm`, in blocks. Protocol constant,
/// *proposed* at 100 in v4 — it is not yet ratified by any implementation.
pub const TAU_CONFIRM_BLOCKS: f64 = 100.0;

/// Observer weight `w_obs` from Eq. 18. Protocol constant, proposed = 1.0.
/// Controls how much poor coverage inflates the stress reading.
pub const W_OBS: f64 = 1.0;

/// Finality confidence `epsilon` used to derive `D_reorg` (Eq. 24).
pub const FINALITY_EPSILON: f64 = 1e-6;

/// Default gauge window `tau`, in seconds. Paper Table 1 lists this as a
/// protocol constant, but §13.2 is candid that dividing by it is a
/// normalisation choice, not a derivation.
pub const DEFAULT_TAU_SECS: f64 = 60.0;

/// Lower clamp on `Lambda_commit` before dividing in Eq. 25. The paper calls
/// this "a deliberate engineering choice, not a physical prediction", and notes
/// it caps `K_enhanced <= 100 * K_base`.
pub const LAMBDA_COMMIT_MIN: f64 = 0.01;

/// The cap implied by [`LAMBDA_COMMIT_MIN`].
pub const K_ENHANCED_MAX_RATIO: f64 = 1.0 / LAMBDA_COMMIT_MIN;

/// Fallback network size when no estimator is wired. Purely a placeholder —
/// see paper limitation L11. Wrapping it in
/// [`crate::observables::NetworkSize::Placeholder`] is what marks any result
/// that touches it as untrustworthy.
pub const N_TOTAL_FALLBACK: u64 = 50;

/// Phase boundary: `K >= 5` is "approaching".
///
/// Paper limitation L9: these thresholds "have not been empirically validated"
/// against real incidents. Treat them as a starting calibration.
pub const PHASE_APPROACHING: f64 = 5.0;

/// Phase boundary: `K >= 10` is "critical". Same caveat as
/// [`PHASE_APPROACHING`].
pub const PHASE_CRITICAL: f64 = 10.0;

/// Maximum realistic reorg depth `D_reorg = kappa * ceil(log2(1/epsilon))`
/// (Eq. 24). At `kappa = 18`, `epsilon = 1e-6` this is `18 * 20 = 360`.
pub fn reorg_depth_bound(kappa: f64, epsilon: f64) -> u64 {
    if !(kappa.is_finite() && kappa > 0.0) || !(epsilon > 0.0 && epsilon < 1.0) {
        return 0;
    }
    let bits = (1.0 / epsilon).log2().ceil();
    (kappa * bits).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorg_bound_matches_paper_worked_example() {
        // kappa = 18, epsilon = 1e-6 -> ceil(log2(1e6)) = ceil(19.93) = 20
        // 18 * 20 = 360
        assert_eq!(reorg_depth_bound(18.0, 1e-6), 360);
    }

    #[test]
    fn reorg_bound_rejects_nonsense_inputs() {
        assert_eq!(reorg_depth_bound(0.0, 1e-6), 0);
        assert_eq!(reorg_depth_bound(18.0, 0.0), 0);
        assert_eq!(reorg_depth_bound(18.0, 1.0), 0);
        assert_eq!(reorg_depth_bound(f64::NAN, 1e-6), 0);
    }

    #[test]
    fn tighter_finality_demands_deeper_confirmation() {
        assert!(reorg_depth_bound(18.0, 1e-9) > reorg_depth_bound(18.0, 1e-6));
    }

    #[test]
    fn enhanced_cap_follows_from_lambda_clamp() {
        assert!((K_ENHANCED_MAX_RATIO - 100.0).abs() < 1e-12);
    }
}
