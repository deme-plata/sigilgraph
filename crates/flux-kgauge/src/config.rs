//! Tunables. Every field is something the paper flags as a calibration rather
//! than a derivation, so it belongs in config, not in a `const` buried in a hot
//! path.

use serde::{Deserialize, Serialize};

use crate::constants::{
    FINALITY_EPSILON, KAPPA, LAMBDA_COMMIT_MIN, PHASE_APPROACHING, PHASE_CRITICAL,
    TAU_CONFIRM_BLOCKS, W_OBS,
};

/// Phase boundaries on the gauge value.
///
/// Paper limitation L9 is explicit that 5.0 and 10.0 "have not been empirically
/// validated" — no incident has been replayed against them. Keep them
/// adjustable so a chain that calibrates against real reorgs can say so.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhaseThresholds {
    pub approaching: f64,
    pub critical: f64,
}

impl Default for PhaseThresholds {
    fn default() -> Self {
        Self { approaching: PHASE_APPROACHING, critical: PHASE_CRITICAL }
    }
}

impl PhaseThresholds {
    pub fn new(approaching: f64, critical: f64) -> Self {
        Self { approaching, critical: critical.max(approaching) }
    }
}

/// Which gauge value drives the phase decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseDriver {
    /// Use `K_base` (Eq. 10). The paper's own recommendation: "The base K-gauge
    /// remains the operational tool; K_enhanced is presented as a proposed
    /// upgrade."
    Base,
    /// Use `K_enhanced` (Eq. 25). Catches Sybil partition and shallow-tip
    /// scenarios the base gauge is blind to, at the cost of importing a
    /// hardcoded `n_total`.
    Enhanced,
}

impl Default for PhaseDriver {
    fn default() -> Self {
        PhaseDriver::Base
    }
}

/// Everything the gauge needs that is not an observation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GaugeConfig {
    /// Expected block rate in blocks/second, used for the block-rate-deviation
    /// channel. This is chain-specific: a 1 bps chain and a 6 bps chain have
    /// completely different "normal", and hardcoding 1 bps silently inflates
    /// the gauge on a fast chain.
    pub target_block_rate_bps: f64,
    /// DAG-Knight `kappa`.
    pub kappa: f64,
    /// Target confirmation depth in blocks (Eq. 20 denominator, with `kappa`).
    pub tau_confirm_blocks: f64,
    /// Observer weight in Eq. 18 / Eq. 25.
    pub w_obs: f64,
    /// Finality confidence used to derive `D_reorg` (Eq. 24).
    pub finality_epsilon: f64,
    /// Lower clamp on `Lambda_commit` before the Eq. 25 division.
    pub lambda_commit_min: f64,
    pub thresholds: PhaseThresholds,
    pub phase_driver: PhaseDriver,
}

impl Default for GaugeConfig {
    fn default() -> Self {
        Self {
            target_block_rate_bps: 1.0,
            kappa: KAPPA,
            tau_confirm_blocks: TAU_CONFIRM_BLOCKS,
            w_obs: W_OBS,
            finality_epsilon: FINALITY_EPSILON,
            lambda_commit_min: LAMBDA_COMMIT_MIN,
            thresholds: PhaseThresholds::default(),
            phase_driver: PhaseDriver::default(),
        }
    }
}

impl GaugeConfig {
    /// Quillon Graph mainnet: roughly one block per second.
    pub fn quillon() -> Self {
        Self { target_block_rate_bps: 1.0, ..Self::default() }
    }

    /// SIGIL, from a live measurement on Epsilon (2026-08-28): 50 blocks in a
    /// 60-second window = 0.83 blocks/second at the steady-state tip.
    ///
    /// The often-quoted 6.28 blocks/s is a *catch-up* rate — what the chain
    /// does while a node is closing a backlog, and the figure behind the
    /// "512 blocks / ~81.5s finality lag" arithmetic. Using it as the steady
    /// -state target makes the block-rate channel report a deviation of 0.87
    /// on a perfectly healthy chain.
    ///
    /// Re-derive this whenever block production changes:
    /// `(height_now - height_60s_ago) / 60`. The gauge raises the
    /// `target_rate_mismatch` caveat when the observed rate is more than 5x
    /// off the configured target, which is how the 6.28 figure was caught.
    pub fn sigil() -> Self {
        Self { target_block_rate_bps: 0.83, ..Self::default() }
    }

    pub fn with_target_block_rate(mut self, bps: f64) -> Self {
        self.target_block_rate_bps = bps;
        self
    }

    pub fn with_phase_driver(mut self, driver: PhaseDriver) -> Self {
        self.phase_driver = driver;
        self
    }

    pub fn with_thresholds(mut self, t: PhaseThresholds) -> Self {
        self.thresholds = t;
        self
    }

    /// `D_reorg` for this configuration.
    pub fn reorg_depth_bound(&self) -> u64 {
        crate::constants::reorg_depth_bound(self.kappa, self.finality_epsilon)
    }

    /// The `kappa * tau_confirm` scale in Eq. 20.
    pub fn commitment_scale(&self) -> f64 {
        (self.kappa * self.tau_confirm_blocks).max(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reproduce_paper_numbers() {
        let c = GaugeConfig::default();
        assert_eq!(c.reorg_depth_bound(), 360);
        assert!((c.commitment_scale() - 1800.0).abs() < 1e-9);
        assert!((c.thresholds.approaching - 5.0).abs() < 1e-12);
        assert!((c.thresholds.critical - 10.0).abs() < 1e-12);
    }

    #[test]
    fn chain_presets_carry_their_own_measured_block_rate() {
        // Presets exist precisely so a chain is never judged against another
        // chain's normal.
        assert_ne!(
            GaugeConfig::sigil().target_block_rate_bps,
            GaugeConfig::quillon().target_block_rate_bps
        );
        // Both must be positive, or the block-rate channel silently drops out.
        assert!(GaugeConfig::sigil().target_block_rate_bps > 0.0);
        assert!(GaugeConfig::quillon().target_block_rate_bps > 0.0);
    }

    #[test]
    fn thresholds_cannot_invert() {
        let t = PhaseThresholds::new(10.0, 2.0);
        assert!(t.critical >= t.approaching);
    }

    #[test]
    fn default_driver_is_base_per_paper_recommendation() {
        assert_eq!(GaugeConfig::default().phase_driver, PhaseDriver::Base);
    }
}
