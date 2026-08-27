//! # flux-kgauge — the DAG-Knight consensus observability gauge
//!
//! An implementation of the K-gauge from *The Theoretical Minimum for
//! Blockchain Consensus* v4 (Kristensen, April 2026), built to be reusable
//! across Quillon Graph, SIGIL, and a simulator. Pure arithmetic over a plain
//! observation struct: no chain types, no HTTP, no async, no I/O.
//!
//! ## What the gauge is
//!
//! One number that says how stressed a node's view of consensus is:
//!
//! ```text
//!            2*pi * sqrt(dH * ds)
//!     K = ------------------------                                     (10)
//!                   tau
//! ```
//!
//! `dH` sums the things going wrong *locally* — mining solutions being
//! rejected, P2P traffic lopsided, peers churning. `ds` sums the things going
//! wrong in *relation to everyone else* — height divergence from the network,
//! block rate off target. The geometric mean is the interesting choice: it is
//! zero when either side is zero, so a purely local problem or a purely network
//! problem does not raise an alarm on its own. You need both.
//!
//! The `2*pi` and the `/tau` are, in the paper's own words, "empirical scaling
//! choices... a calibration, not a derivation". Earlier versions dressed this
//! up as a Heisenberg uncertainty relation with an `hbar` in it. v4 removed
//! that, and so does this crate.
//!
//! ## The v4 enhancement
//!
//! Two blind spots motivated the v4 additions, both cases where every counter
//! reads healthy and the node is nonetheless in trouble:
//!
//! * **A Sybil partition.** The adversary owns all your peers and feeds you a
//!   consistent, healthy-looking view. Your gauge reads zero. The observer
//!   coverage factor `Omega_node` (Eq. 17) catches this by asking not "do my
//!   metrics look good" but "how much of the network am I even looking at".
//! * **A shallow tip.** Just after a restart the chain tip has almost nothing
//!   built on it, so the ordering you are serving could still change.
//!   `Lambda_commit` (Eq. 20) catches this by measuring how many descendants
//!   have accumulated.
//!
//! Together (Eq. 25):
//!
//! ```text
//!                  K_base
//!     K_enhanced = ------- * (1 + (1 - Omega_node) * w_obs)             (25)
//!                  Lambda
//! ```
//!
//! ## What this crate adds beyond a transcription
//!
//! The paper's discipline is that every claim is tagged by how it was obtained,
//! and its Table 1 catalogs which inputs are real measurements and which are
//! hardcoded placeholders. Here that catalog is in the type system rather than
//! in prose:
//!
//! * [`provenance::Provenance`] tags every value `Measured` / `Derived` /
//!   `Protocol` / `Placeholder` / `Unavailable`, and a derived value is never
//!   more trustworthy than its worst input.
//! * A channel with no data reports [`None`], never `0.0`. "We never heard a
//!   peer height" and "we are perfectly in sync" must not produce the same
//!   number.
//! * [`base::Confidence`] separates `K = 0` because everything is healthy from
//!   `K = 0` because nothing happened.
//! * [`commitment::Commitment::lambda_ceiling`] reports the largest `Lambda`
//!   the current measurement setup could *ever* produce, so a `Lambda` pinned
//!   by window length is visible as such instead of being read as chain health.
//! * `f_irrev` (Eq. 23) returns [`None`] when the window is shorter than
//!   `D_reorg`, because in that regime the answer is zero by construction.
//! * [`anticone`] measures DAG-Knight's `k` and keeps it strictly separate from
//!   the K-gauge's `K` — two different quantities that share a letter and move
//!   in opposite directions.
//! * Every reading carries a [`gauge::Caveat`] list with machine-stable codes.
//!
//! ## Using it
//!
//! ```
//! use flux_kgauge::{
//!     GaugeConfig, KGauge, NetworkSize, Observables, CounterSample,
//! };
//!
//! let obs = Observables {
//!     previous: CounterSample {
//!         mining_submitted: 1_000, mining_accepted: 990,
//!         p2p_bytes_in: 10_000, p2p_bytes_out: 10_000,
//!         peer_count: 6, local_height: 201_000, network_height: 201_000,
//!     },
//!     current: CounterSample {
//!         mining_submitted: 1_100, mining_accepted: 1_089,
//!         p2p_bytes_in: 20_000, p2p_bytes_out: 20_100,
//!         peer_count: 6, local_height: 201_377, network_height: 201_377,
//!     },
//!     window_secs: 60.0,
//!     network_size: NetworkSize::Known(8),   // say what you actually know
//!     ..Observables::default()
//! };
//!
//! let mut gauge = KGauge::new(GaugeConfig::sigil());
//! let report = gauge.observe(&obs);
//!
//! println!("{}", report.render());
//! assert!(report.base.k_base.is_some());
//! ```
//!
//! ## What this crate does not do
//!
//! It does not implement the Hamiltonian, the effective temperature, the phase
//! diagram, or the diffusion model from Parts II and III of the paper. Those
//! depend on quantities (`delta`, `f/n`, mesh degree, anticone size) that are
//! hardcoded placeholders in every implementation the paper surveys. The
//! K-gauge is the part the paper itself calls "the only component where all
//! inputs are genuinely measured from live network state", and it is the part
//! worth shipping.
//!
//! It also proves nothing. The one exact theorem in the paper — the Ground
//! State Theorem, that the Hamiltonian's minimum is the PHANTOM ordering — is
//! about the Hamiltonian, which is not implemented here.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod anticone;
pub mod base;
pub mod commitment;
pub mod config;
pub mod constants;
pub mod gauge;
pub mod observables;
pub mod observer;
pub mod phase;
pub mod provenance;

pub use anticone::{AnticoneMeasure, DagShape};
pub use base::{Aggregate, BaseGauge, Channel, Confidence};
pub use commitment::{Commitment, CommitmentBasis};
pub use config::{GaugeConfig, PhaseDriver, PhaseThresholds};
pub use constants::reorg_depth_bound;
pub use gauge::{Caveat, GaugeReport, KGauge};
pub use observables::{BlockFact, BlockWindow, CounterSample, NetworkSize, Observables};
pub use observer::{omega_node, ObserverCoverage};
pub use phase::{Phase, PhaseTracker, Tuning};
pub use provenance::{Provenance, Tracked};

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Reproduce the paper's Table 6 scenario responses.
    ///
    /// Table 6 fixes `K_base` and varies `Omega` and `Lambda`, so we drive the
    /// enhancement arithmetic directly rather than manufacturing observables
    /// that happen to land on those base values.
    fn enhanced(k_base: f64, omega: f64, lambda: f64, w_obs: f64) -> f64 {
        let l = lambda.max(constants::LAMBDA_COMMIT_MIN);
        (k_base / l) * (1.0 + (1.0 - omega) * w_obs)
    }

    #[test]
    fn table6_healthy_row() {
        // K_base 0.19, Omega 0.92, Lambda 0.95 -> K_enh 0.22, Stable
        let k = enhanced(0.19, 0.92, 0.95, 1.0);
        assert!((k - 0.216).abs() < 0.01, "got {k}");
        assert_eq!(phase::classify(k, &PhaseThresholds::default()), Phase::Stable);
    }

    #[test]
    fn table6_sybil_row() {
        // Omega collapses to 0.15 -> K_enh 0.37, still Stable but 1.7x higher
        let k = enhanced(0.19, 0.15, 0.95, 1.0);
        assert!((k - 0.370).abs() < 0.01, "got {k}");
        assert_eq!(phase::classify(k, &PhaseThresholds::default()), Phase::Stable);
    }

    #[test]
    fn table6_fresh_restart_row() {
        // Lambda collapses to 0.05 -> K_enh 4.18
        let k = enhanced(0.19, 0.92, 0.05, 1.0);
        assert!((k - 4.104).abs() < 0.1, "got {k}");
        assert_eq!(phase::classify(k, &PhaseThresholds::default()), Phase::Stable);
    }

    #[test]
    fn table6_sybil_plus_shallow_row() {
        // Both degrade -> K_enh 7.03, crosses into Approaching
        let k = enhanced(0.19, 0.15, 0.05, 1.0);
        assert!((k - 7.03).abs() < 0.1, "got {k}");
        assert_eq!(
            phase::classify(k, &PhaseThresholds::default()),
            Phase::Approaching
        );
    }

    #[test]
    fn table6_extreme_plus_sybil_plus_shallow_row() {
        // K_base 1.27 under all three -> K_enh 47.0, Critical
        let k = enhanced(1.27, 0.15, 0.05, 1.0);
        assert!((k - 47.0).abs() < 0.5, "got {k}");
        assert_eq!(phase::classify(k, &PhaseThresholds::default()), Phase::Critical);
    }

    #[test]
    fn enhancement_is_transparent_when_both_terms_are_good() {
        let k_base = 0.42;
        let k = enhanced(k_base, 1.0, 1.0, 1.0);
        assert!((k - k_base).abs() < 1e-12);
    }

    /// The two k's move in opposite directions on the same event, which is why
    /// the crate never mixes them.
    #[test]
    fn anticone_k_and_gauge_k_are_independent() {
        // A wide DAG (high anticone k) on a node whose own counters are clean
        // (low gauge K).
        let wide = BlockWindow::new(
            (0..40)
                .map(|i| BlockFact {
                    height: 1_000 + i,
                    producer: [(i % 5) as u8; 32],
                    merge_parent_count: 25,
                    blue_score: Some(i),
                    is_blue: Some(true),
                })
                .collect(),
        );
        let obs = Observables {
            previous: CounterSample {
                mining_submitted: 10,
                mining_accepted: 10,
                p2p_bytes_in: 100,
                p2p_bytes_out: 100,
                peer_count: 8,
                local_height: 1_000,
                network_height: 1_000,
            },
            current: CounterSample {
                mining_submitted: 20,
                mining_accepted: 20,
                p2p_bytes_in: 200,
                p2p_bytes_out: 200,
                peer_count: 8,
                local_height: 1_060,
                network_height: 1_060,
            },
            window_secs: 60.0,
            window: wide,
            network_size: NetworkSize::Known(8),
            kappa: 18.0,
        };

        let mut g = KGauge::new(GaugeConfig::quillon());
        let r = g.observe(&obs);
        assert_eq!(r.anticone.shape, DagShape::WideDag);
        assert_eq!(r.anticone.k_observed_max, 25);
        assert!(r.base.value_or_zero() < 1e-9, "gauge K should be ~0");
        assert_eq!(r.phase, Phase::Stable);
    }

    /// The scenario Viktor asked about: two well-connected servers, seen
    /// honestly versus seen through a hardcoded network-size guess.
    #[test]
    fn small_well_connected_fleet_reads_healthy_when_n_total_is_honest() {
        let window = BlockWindow::new(
            (0..377)
                .map(|i| BlockFact {
                    height: 201_000 + i,
                    producer: [(i % 2) as u8; 32],
                    merge_parent_count: 0,
                    blue_score: Some(i),
                    is_blue: Some(true),
                })
                .collect(),
        );
        let mk = |size: NetworkSize| Observables {
            previous: CounterSample {
                mining_submitted: 1_000,
                mining_accepted: 995,
                p2p_bytes_in: 1_000_000,
                p2p_bytes_out: 1_000_000,
                peer_count: 6,
                local_height: 201_000,
                network_height: 201_000,
            },
            current: CounterSample {
                mining_submitted: 1_100,
                mining_accepted: 1_094,
                p2p_bytes_in: 2_000_000,
                p2p_bytes_out: 2_000_000,
                peer_count: 6,
                local_height: 201_377,
                network_height: 201_377,
            },
            window_secs: 60.0,
            window: window.clone(),
            network_size: size,
            kappa: 18.0,
        };

        let mut g = KGauge::new(
            GaugeConfig::sigil().with_phase_driver(PhaseDriver::Enhanced),
        );
        let honest = g.observe(&mk(NetworkSize::Known(8)));
        let guessed = g.observe(&mk(NetworkSize::Placeholder(50)));

        // Same chain, same counters. Only the network-size assumption differs.
        assert!(honest.observer.omega > 0.5);
        assert!(guessed.observer.omega < 0.15);
        assert!(guessed.k_enhanced > honest.k_enhanced);
        assert!(honest.is_actionable());
        assert!(!guessed.is_actionable());
    }
}
