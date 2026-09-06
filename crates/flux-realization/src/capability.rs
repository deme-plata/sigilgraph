//! The capability core: compute, energy, verifiability, availability,
//! expandability — combined as a geometric mean over **only the terms that were
//! actually measured**.
//!
//! ## Why a geometric mean
//!
//! An arithmetic mean lets a design buy its way out of a zero. Ten times the
//! compute would offset unverifiable results, and the score would go up while
//! the system became worthless. The geometric mean refuses: one zero zeroes the
//! product. That is the correct semantics here, because these five are not
//! interchangeable goods, they are *conjunctive requirements*. A datacenter that
//! computes enormously and can prove nothing is not 80% of a datacenter.
//!
//! It also makes the terms scale-free. Doubling compute and halving efficiency
//! leaves the core unchanged, which is what you want from a figure of merit that
//! has to compare a 50 MW modular design against a 1 GW monument.
//!
//! ## The honest part: a missing term is not a zero
//!
//! SIGIL's node exposes no power telemetry, so `energy` is genuinely
//! unmeasurable from the chain today. There are two dishonest ways to handle
//! that and one honest one:
//!
//! * substitute a plausible number and let it look measured — this is how a
//!   gauge starts lying;
//! * call it 0.0 — which zeroes the product and reports a healthy system as
//!   dead;
//! * **drop it from the mean and say which terms the mean was taken over.**
//!
//! [`Capability::core`] does the third. A four-term geometric mean with
//! `excluded: ["energy"]` attached is an honest reading. A five-term mean with a
//! guessed fifth is not, however careful the guess.

use flux_kgauge::provenance::{Provenance, Tracked};
use serde::{Deserialize, Serialize};

/// The five capability terms. Each is a dimensionless score in `[0, 1]` — a
/// *fraction of a stated reference*, never a raw magnitude, so that the
/// geometric mean is meaningful across wildly different scales.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// `C` — verified useful operations per second, against a reference rate.
    /// Note "verified": raw FLOPS that nobody checked belong in `verifiable`'s
    /// denominator, not here.
    pub compute: Tracked<f64>,
    /// `E` — useful work per joule, against a reference efficiency.
    pub energy: Tracked<f64>,
    /// `V` — what fraction of claimed work can an independent party check?
    pub verifiable: Tracked<f64>,
    /// `A` — probability the system is operational.
    pub availability: Tracked<f64>,
    /// `X` — how cheaply the design replicates. A design needing a unique
    /// mountain approaches 0; a containerized module approaches 1.
    pub expandable: Tracked<f64>,
}

/// Outcome of folding the five terms into one number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityCore {
    /// The geometric mean over the included terms.
    pub core: f64,
    /// Which terms were included, in order.
    pub included: Vec<String>,
    /// Which terms were dropped because their provenance was
    /// [`Provenance::Unavailable`], and therefore what the reading is blind to.
    pub excluded: Vec<String>,
    /// Terms that were included and are exactly zero — the ones actually
    /// killing the score, called out because a zero product otherwise tells you
    /// nothing about which factor did it.
    pub zeroed: Vec<String>,
    /// Worst provenance among the included terms.
    pub provenance: Provenance,
}

impl Capability {
    fn entries(&self) -> [(&'static str, &Tracked<f64>); 5] {
        [
            ("compute", &self.compute),
            ("energy", &self.energy),
            ("verifiable", &self.verifiable),
            ("availability", &self.availability),
            ("expandable", &self.expandable),
        ]
    }

    /// Geometric mean over the terms whose provenance is not
    /// [`Provenance::Unavailable`].
    ///
    /// With no usable terms at all the core is `0.0` with provenance
    /// `Unavailable` — a reading that must not be acted on, rather than a
    /// vacuous `1.0` from an empty product.
    pub fn core(&self) -> CapabilityCore {
        let mut included = Vec::new();
        let mut excluded = Vec::new();
        let mut zeroed = Vec::new();
        let mut provs = Vec::new();
        let mut log_sum = 0.0_f64;
        let mut any_zero = false;

        for (name, t) in self.entries() {
            if t.provenance == Provenance::Unavailable {
                excluded.push(name.to_string());
                continue;
            }
            let v = if t.value.is_finite() { t.value.clamp(0.0, 1.0) } else { 0.0 };
            included.push(name.to_string());
            provs.push(t.provenance);
            if v <= 0.0 {
                any_zero = true;
                zeroed.push(name.to_string());
            } else {
                log_sum += v.ln();
            }
        }

        if included.is_empty() {
            return CapabilityCore {
                core: 0.0,
                included,
                excluded,
                zeroed,
                provenance: Provenance::Unavailable,
            };
        }
        // Computed in log space: five factors near 1e-3 each underflow a naive
        // product long before they underflow the sum of their logarithms.
        let core = if any_zero { 0.0 } else { (log_sum / included.len() as f64).exp() };
        CapabilityCore { core, included, excluded, zeroed, provenance: Provenance::worst_of(&provs) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(c: f64, e: f64, v: f64, a: f64, x: f64) -> Capability {
        Capability {
            compute: Tracked::measured(c, "t"),
            energy: Tracked::measured(e, "t"),
            verifiable: Tracked::measured(v, "t"),
            availability: Tracked::measured(a, "t"),
            expandable: Tracked::measured(x, "t"),
        }
    }

    #[test]
    fn one_zero_zeroes_the_core_and_names_itself() {
        let core = full(1.0, 1.0, 0.0, 1.0, 1.0).core();
        assert_eq!(core.core, 0.0);
        assert_eq!(core.zeroed, vec!["verifiable".to_string()]);
    }

    #[test]
    fn compute_cannot_buy_its_way_out_of_unverifiability() {
        let honest = full(0.1, 0.5, 1.0, 1.0, 1.0).core().core;
        let blind = full(1.0, 1.0, 0.0, 1.0, 1.0).core().core;
        assert!(honest > blind, "a modest verified system beats an enormous unverified one");
    }

    #[test]
    fn geometric_mean_is_scale_free() {
        // Double compute, halve efficiency: same core.
        let a = full(0.4, 0.4, 0.9, 0.9, 0.9).core().core;
        let b = full(0.8, 0.2, 0.9, 0.9, 0.9).core().core;
        assert!((a - b).abs() < 1e-12, "{a} vs {b}");
    }

    #[test]
    fn an_unavailable_term_is_dropped_not_zeroed() {
        let mut cap = full(0.8, 0.0, 0.8, 0.8, 0.8);
        cap.energy = Tracked::unavailable(0.0, "no power telemetry on sigil-api");
        let core = cap.core();
        assert!(core.core > 0.0, "a missing measurement must not report a dead system");
        assert_eq!(core.excluded, vec!["energy".to_string()]);
        assert_eq!(core.included.len(), 4);
        // Four equal terms of 0.8 -> exactly 0.8.
        assert!((core.core - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_reading_with_nothing_measurable_is_unavailable_not_perfect() {
        let cap = Capability {
            compute: Tracked::unavailable(0.0, ""),
            energy: Tracked::unavailable(0.0, ""),
            verifiable: Tracked::unavailable(0.0, ""),
            availability: Tracked::unavailable(0.0, ""),
            expandable: Tracked::unavailable(0.0, ""),
        };
        let core = cap.core();
        assert_eq!(core.core, 0.0);
        assert_eq!(core.provenance, Provenance::Unavailable);
        assert_eq!(core.excluded.len(), 5);
    }

    #[test]
    fn core_is_no_more_trustworthy_than_its_worst_included_term() {
        let mut cap = full(0.9, 0.9, 0.9, 0.9, 0.9);
        cap.expandable = Tracked::placeholder(0.9, "guessed");
        assert_eq!(cap.core().provenance, Provenance::Placeholder);
    }

    #[test]
    fn tiny_terms_do_not_underflow() {
        let core = full(1e-60, 1e-60, 1e-60, 1e-60, 1e-60).core();
        assert!(core.core > 0.0, "log-space keeps this finite: {}", core.core);
        assert!((core.core - 1e-60).abs() / 1e-60 < 1e-9);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_trusted() {
        let core = full(50.0, 1.0, 1.0, 1.0, 1.0).core();
        assert!((core.core - 1.0).abs() < 1e-12, "a term above 1 cannot inflate the core");
    }
}
