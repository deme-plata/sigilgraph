//! Phase classification, transition detection, and the tuning knobs a phase
//! change should move.
//!
//! The phases are a *structural analogy* in the paper's own taxonomy — level 3,
//! the weakest tier. "Critical" does not mean the system is at a
//! thermodynamic critical point; it means the composite stress score crossed a
//! number somebody picked. Paper limitation L9 says those numbers "have not
//! been empirically validated". They are a starting calibration and should be
//! moved once real incidents have been replayed against them.
//!
//! What *is* real here is the shape of the response: a rising stress score
//! should make the node accept less speculative work and demand more proof
//! before committing. That direction is sound even if the exact boundary is not.

use serde::{Deserialize, Serialize};

use crate::config::PhaseThresholds;

/// Where the gauge value sits relative to the thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stable,
    Approaching,
    Critical,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Stable => "stable",
            Phase::Approaching => "approaching",
            Phase::Critical => "critical",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a gauge value.
pub fn classify(k: f64, t: &PhaseThresholds) -> Phase {
    if !k.is_finite() {
        return Phase::Stable;
    }
    if k >= t.critical {
        Phase::Critical
    } else if k >= t.approaching {
        Phase::Approaching
    } else {
        Phase::Stable
    }
}

/// What the node should do differently in each phase.
///
/// These are policy, not physics. They are here so that a phase change has a
/// concrete effect rather than only lighting a dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tuning {
    /// Cap on mining solutions accepted into one block.
    pub max_solutions_per_block: u64,
    /// Multiplier on VDF difficulty — more sequential work demanded per block
    /// when the network looks stressed.
    pub vdf_multiplier: f64,
    /// How long a mining challenge stays valid.
    pub challenge_expiry_secs: u64,
}

impl Tuning {
    pub fn for_phase(p: Phase) -> Self {
        match p {
            Phase::Stable => Tuning {
                max_solutions_per_block: 250,
                vdf_multiplier: 1.0,
                challenge_expiry_secs: 120,
            },
            Phase::Approaching => Tuning {
                max_solutions_per_block: 150,
                vdf_multiplier: 1.25,
                challenge_expiry_secs: 90,
            },
            Phase::Critical => Tuning {
                max_solutions_per_block: 50,
                vdf_multiplier: 1.5,
                challenge_expiry_secs: 60,
            },
        }
    }
}

/// Rolling history of gauge readings, for trend and transition detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseTracker {
    history: Vec<f64>,
    max_history: usize,
    last_phase: Phase,
    transitions: u64,
    rounds: u64,
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self::new(256)
    }
}

impl PhaseTracker {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: Vec::new(),
            max_history: max_history.max(2),
            last_phase: Phase::Stable,
            transitions: 0,
            rounds: 0,
        }
    }

    /// Record a reading. Returns `(new_phase, previous_phase)`.
    pub fn observe(&mut self, k: f64, t: &PhaseThresholds) -> (Phase, Phase) {
        let k = if k.is_finite() { k } else { 0.0 };
        self.history.push(k);
        // Ring behaviour without the cost of a front-remove on a Vec: drain the
        // oldest quarter when full, so amortised cost stays O(1) per push.
        if self.history.len() > self.max_history {
            let drop = self.max_history / 4;
            self.history.drain(0..drop.max(1));
        }
        self.rounds += 1;

        let previous = self.last_phase;
        let new = classify(k, t);
        if new != previous {
            self.transitions += 1;
        }
        self.last_phase = new;
        (new, previous)
    }

    /// Least-squares slope over the last `n` readings. Positive means stress is
    /// rising.
    pub fn trend(&self, n: usize) -> f64 {
        let n = n.max(2);
        let start = self.history.len().saturating_sub(n);
        let recent = &self.history[start..];
        if recent.len() < 2 {
            return 0.0;
        }
        let len = recent.len() as f64;
        let sum_x: f64 = (0..recent.len()).map(|i| i as f64).sum();
        let sum_y: f64 = recent.iter().sum();
        let sum_xy: f64 = recent.iter().enumerate().map(|(i, &y)| i as f64 * y).sum();
        let sum_x2: f64 = (0..recent.len()).map(|i| (i as f64).powi(2)).sum();
        let denom = len * sum_x2 - sum_x * sum_x;
        if denom.abs() < 1e-15 {
            0.0
        } else {
            (len * sum_xy - sum_x * sum_y) / denom
        }
    }

    /// Sample standard deviation of the history — how jumpy the gauge is.
    pub fn volatility(&self) -> f64 {
        if self.history.len() < 2 {
            return 0.0;
        }
        let n = self.history.len() as f64;
        let mean = self.history.iter().sum::<f64>() / n;
        let var = self
            .history
            .iter()
            .map(|x| (x - mean).powi(2))
            .sum::<f64>()
            / (n - 1.0);
        var.sqrt()
    }

    pub fn history(&self) -> &[f64] {
        &self.history
    }

    pub fn last_phase(&self) -> Phase {
        self.last_phase
    }

    pub fn transitions(&self) -> u64 {
        self.transitions
    }

    pub fn rounds(&self) -> u64 {
        self.rounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_uses_thresholds() {
        let t = PhaseThresholds::default();
        assert_eq!(classify(0.19, &t), Phase::Stable);
        assert_eq!(classify(4.99, &t), Phase::Stable);
        assert_eq!(classify(5.0, &t), Phase::Approaching);
        assert_eq!(classify(7.03, &t), Phase::Approaching);
        assert_eq!(classify(10.0, &t), Phase::Critical);
        assert_eq!(classify(47.0, &t), Phase::Critical);
    }

    #[test]
    fn paper_table6_rows_classify_as_published() {
        let t = PhaseThresholds::default();
        // Kbase 0.19 healthy -> Kenh 0.22 Stable
        assert_eq!(classify(0.22, &t), Phase::Stable);
        // Sybil 0.37 Stable
        assert_eq!(classify(0.37, &t), Phase::Stable);
        // Fresh restart 4.18 Stable
        assert_eq!(classify(4.18, &t), Phase::Stable);
        // Sybil + shallow 7.03 Approaching
        assert_eq!(classify(7.03, &t), Phase::Approaching);
        // Extreme 1.47 Stable
        assert_eq!(classify(1.47, &t), Phase::Stable);
        // Extreme + Sybil + shallow 47.0 Critical
        assert_eq!(classify(47.0, &t), Phase::Critical);
    }

    #[test]
    fn nan_does_not_page_anyone() {
        assert_eq!(classify(f64::NAN, &PhaseThresholds::default()), Phase::Stable);
    }

    #[test]
    fn tuning_tightens_monotonically_with_stress() {
        let s = Tuning::for_phase(Phase::Stable);
        let a = Tuning::for_phase(Phase::Approaching);
        let c = Tuning::for_phase(Phase::Critical);
        assert!(s.max_solutions_per_block > a.max_solutions_per_block);
        assert!(a.max_solutions_per_block > c.max_solutions_per_block);
        assert!(c.vdf_multiplier > a.vdf_multiplier);
        assert!(a.vdf_multiplier > s.vdf_multiplier);
        assert!(c.challenge_expiry_secs < s.challenge_expiry_secs);
    }

    #[test]
    fn tracker_counts_transitions_in_both_directions() {
        let t = PhaseThresholds::default();
        let mut tr = PhaseTracker::new(64);
        tr.observe(0.2, &t); // stable -> stable, no transition
        assert_eq!(tr.transitions(), 0);
        tr.observe(6.0, &t); // -> approaching
        assert_eq!(tr.transitions(), 1);
        tr.observe(12.0, &t); // -> critical
        assert_eq!(tr.transitions(), 2);
        tr.observe(0.1, &t); // -> stable
        assert_eq!(tr.transitions(), 3);
        assert_eq!(tr.rounds(), 4);
    }

    #[test]
    fn trend_signs_are_right() {
        let t = PhaseThresholds::default();
        let mut up = PhaseTracker::new(64);
        for i in 0..6 {
            up.observe(i as f64, &t);
        }
        assert!(up.trend(5) > 0.9);

        let mut down = PhaseTracker::new(64);
        for i in (0..6).rev() {
            down.observe(i as f64, &t);
        }
        assert!(down.trend(5) < -0.9);
    }

    #[test]
    fn flat_history_has_zero_trend_and_volatility() {
        let t = PhaseThresholds::default();
        let mut tr = PhaseTracker::new(64);
        for _ in 0..10 {
            tr.observe(2.0, &t);
        }
        assert!(tr.trend(5).abs() < 1e-12);
        assert!(tr.volatility() < 1e-12);
    }

    #[test]
    fn history_is_bounded() {
        let t = PhaseThresholds::default();
        let mut tr = PhaseTracker::new(16);
        for i in 0..500 {
            tr.observe(i as f64, &t);
        }
        assert!(tr.history().len() <= 16);
        assert_eq!(tr.rounds(), 500);
    }

    #[test]
    fn volatility_detects_a_jumpy_gauge() {
        let t = PhaseThresholds::default();
        let mut tr = PhaseTracker::new(64);
        for i in 0..10 {
            tr.observe(if i % 2 == 0 { 0.0 } else { 10.0 }, &t);
        }
        assert!(tr.volatility() > 4.0);
    }
}
