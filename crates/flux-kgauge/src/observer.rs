//! Observer coverage, Eq. 17 and Eq. 18.
//!
//! The problem this solves: two nodes running identical code on the same chain
//! compute the same formula and both report `K ~ 0`. One is a bootstrap node
//! wired to twelve well-spread peers. The other is behind a Sybil partition and
//! can see two adversary-controlled peers that are feeding it a consistent,
//! healthy-looking lie. The base gauge cannot tell them apart, because every
//! counter it reads looks fine.
//!
//! ```text
//!     Omega_node = 1 - exp(-n_peers / n_total)          (17)
//!     K_obs      = K * (1 + (1 - Omega_node) * w_obs)   (18)
//! ```
//!
//! When coverage is good, `Omega -> 1` and the correction vanishes. When
//! coverage is poor, the correction inflates the stress reading — the node is
//! saying "I cannot see enough of the network to trust my own report."
//!
//! ## The part that matters more than the formula
//!
//! `n_peers` is measured. `n_total` is not. Paper limitation L11: until a DHT
//! crawl provides it, `Omega_node` is "a model with one hardcoded input", and a
//! wrong `n_total` moves the answer a long way. A two-node network measured
//! against a hardcoded `n_total = 50` reports `Omega = 0.04` and screams
//! "isolated" while actually seeing *the entire network*. That is why
//! [`crate::observables::NetworkSize`] forces the caller to say which case they
//! are in, and why the provenance of the result degrades to `Placeholder` the
//! moment a guess is used.

use serde::{Deserialize, Serialize};

use crate::observables::NetworkSize;
use crate::provenance::{Provenance, Tracked};
use std::borrow::Cow;

/// Result of the observer-coverage computation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverCoverage {
    /// `Omega_node` in `[0, 1]` (Eq. 17).
    pub omega: f64,
    /// Peers actually connected.
    pub n_peers: u64,
    /// Network size used, and how it was obtained.
    pub n_total: f64,
    /// `1 + (1 - Omega) * w_obs`, the multiplier applied in Eq. 18 / Eq. 25.
    pub correction: f64,
    pub provenance: Provenance,
    /// True when `n_total` was a guess. If this is set, do not page anyone on
    /// the strength of `omega` alone.
    pub n_total_is_guess: bool,
}

impl ObserverCoverage {
    pub fn tracked_correction(&self) -> Tracked<f64> {
        Tracked {
            value: self.correction,
            provenance: self.provenance,
            note: Cow::Borrowed("observer correction 1 + (1 - Omega)*w_obs, Eq. 18"),
        }
    }
}

/// Eq. 17, bare. `Omega = 1 - exp(-n_peers / n_total)`.
///
/// Returns 0.0 for a node with no peers, and for a nonsensical `n_total`.
pub fn omega_node(n_peers: f64, n_total: f64) -> f64 {
    if !n_peers.is_finite() || !n_total.is_finite() || n_total <= 0.0 || n_peers <= 0.0 {
        return 0.0;
    }
    (1.0 - (-n_peers / n_total).exp()).clamp(0.0, 1.0)
}

/// Compute coverage and the Eq. 18 correction factor.
pub fn compute(n_peers: u64, network_size: NetworkSize, w_obs: f64) -> ObserverCoverage {
    let n_total_tracked = network_size.tracked();
    let n_total = n_total_tracked.value;
    let omega = omega_node(n_peers as f64, n_total);
    let w = if w_obs.is_finite() && w_obs >= 0.0 { w_obs } else { 0.0 };
    let correction = 1.0 + (1.0 - omega) * w;

    // The peer count is measured; the network size may not be. The answer is
    // only as good as the worse of the two.
    let provenance = Provenance::Measured.worst(n_total_tracked.provenance);

    ObserverCoverage {
        omega,
        n_peers,
        n_total,
        correction,
        provenance,
        n_total_is_guess: matches!(network_size, NetworkSize::Placeholder(_)),
    }
}

/// Mesh-quality-weighted coverage — the paper's own "where it fails" note,
/// implemented.
///
/// Eq. 17 counts every peer as one. In practice three well-placed bootstrap
/// peers give better coverage than ten peers behind the same NAT, and a Sybil
/// partition is precisely the case where peer *count* is high and peer
/// *independence* is nil. Supplying a quality weight in `[0, 1]` per peer
/// replaces `n_peers` with the effective peer count `sum(w_i)`.
///
/// This is an **extension beyond the paper**, not a result from it: the weights
/// themselves have no validated derivation. Marked `Derived` at best, and
/// inherits `Placeholder` from `n_total` as usual.
pub fn compute_weighted(
    peer_weights: &[f64],
    network_size: NetworkSize,
    w_obs: f64,
) -> ObserverCoverage {
    let effective: f64 = peer_weights
        .iter()
        .filter(|w| w.is_finite())
        .map(|w| w.clamp(0.0, 1.0))
        .sum();
    let n_total_tracked = network_size.tracked();
    let omega = omega_node(effective, n_total_tracked.value);
    let w = if w_obs.is_finite() && w_obs >= 0.0 { w_obs } else { 0.0 };

    ObserverCoverage {
        omega,
        n_peers: effective.round() as u64,
        n_total: n_total_tracked.value,
        correction: 1.0 + (1.0 - omega) * w,
        // Weights are a modelling choice, so this can never be better than Derived.
        provenance: Provenance::Derived.worst(n_total_tracked.provenance),
        n_total_is_guess: matches!(network_size, NetworkSize::Placeholder(_)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omega_limits() {
        assert!((omega_node(0.0, 50.0) - 0.0).abs() < 1e-12);
        // n_peers == n_total gives the characteristic 1 - 1/e
        let o = omega_node(50.0, 50.0);
        assert!((o - (1.0 - (-1.0f64).exp())).abs() < 1e-12);
        // Far more peers than the estimate saturates at 1
        assert!(omega_node(500.0, 50.0) > 0.9999);
    }

    #[test]
    fn omega_rejects_nonsense() {
        assert_eq!(omega_node(5.0, 0.0), 0.0);
        assert_eq!(omega_node(5.0, -1.0), 0.0);
        assert_eq!(omega_node(f64::NAN, 50.0), 0.0);
    }

    #[test]
    fn paper_table6_healthy_and_sybil_rows() {
        // Table 6 uses Omega = 0.92 healthy, 0.15 under Sybil partition.
        // Healthy: 12 peers of ~5 -> saturated. Reproduce the correction math.
        let healthy = ObserverCoverage {
            omega: 0.92,
            n_peers: 12,
            n_total: 12.0,
            correction: 1.0 + (1.0 - 0.92),
            provenance: Provenance::Measured,
            n_total_is_guess: false,
        };
        assert!((healthy.correction - 1.08).abs() < 1e-12);

        let sybil_correction: f64 = 1.0 + (1.0 - 0.15) * 1.0;
        assert!((sybil_correction - 1.85).abs() < 1e-12);
        // Sybil inflates the reading ~1.71x relative to healthy — the whole point.
        assert!(sybil_correction / healthy.correction > 1.7);
    }

    #[test]
    fn hardcoded_n_total_poisons_provenance() {
        let c = compute(6, NetworkSize::Placeholder(50), 1.0);
        assert_eq!(c.provenance, Provenance::Placeholder);
        assert!(c.n_total_is_guess);
        assert!(!c.provenance.is_operational());
    }

    #[test]
    fn known_n_total_is_operational() {
        let c = compute(6, NetworkSize::Known(8), 1.0);
        assert_eq!(c.provenance, Provenance::Measured);
        assert!(c.provenance.is_operational());
    }

    #[test]
    fn a_small_network_seen_whole_is_not_isolated() {
        // The failure mode the placeholder causes. Six peers on a network that
        // really has eight nodes is near-total coverage; the same six peers
        // measured against a hardcoded 50 look like near-total isolation.
        let honest = compute(6, NetworkSize::Known(8), 1.0);
        let guessed = compute(6, NetworkSize::Placeholder(50), 1.0);
        assert!(honest.omega > 0.5, "honest omega {}", honest.omega);
        assert!(guessed.omega < 0.15, "guessed omega {}", guessed.omega);
        // And the guess therefore inflates the gauge for no reason: with
        // w_obs = 1 the correction runs over [1, 2], so a coverage collapse
        // from 0.53 to 0.11 is a 1.47 -> 1.89 move, ~1.28x.
        let inflation = guessed.correction / honest.correction;
        assert!(inflation > 1.25, "inflation was {inflation}");
    }

    #[test]
    fn weighted_coverage_sees_through_a_sybil_cluster() {
        // Ten peers, but nine are one adversary behind one NAT (weight 0.05)
        // and one is a real independent bootstrap (weight 1.0).
        let mut weights = vec![0.05; 9];
        weights.push(1.0);
        let weighted = compute_weighted(&weights, NetworkSize::Known(12), 1.0);
        let naive = compute(10, NetworkSize::Known(12), 1.0);
        assert!(
            weighted.omega < naive.omega,
            "weighted {} should be below naive {}",
            weighted.omega,
            naive.omega
        );
        assert!(weighted.correction > naive.correction);
        // Never better than Derived: the weights are a model.
        assert_eq!(weighted.provenance, Provenance::Derived);
    }

    #[test]
    fn zero_w_obs_disables_the_correction() {
        let c = compute(1, NetworkSize::Placeholder(1000), 0.0);
        assert!((c.correction - 1.0).abs() < 1e-12);
    }
}
