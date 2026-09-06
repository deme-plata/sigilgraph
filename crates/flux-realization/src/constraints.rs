//! The two halves of "can this design exist, and what is stopping it": the
//! physical-feasibility gate `Theta_phys` and the bottleneck pressure `B(D)`.
//!
//! ## Why these are one module
//!
//! They read the *same* list of constraints and answer two different questions
//! about it. A constraint says "this design needs `required` of resource X and
//! `available` is what exists". From that single fact you get:
//!
//! * **`Theta_phys`** — a hard yes/no. If any constraint marked [`Hardness::Hard`]
//!   is unmet, the design is not in the Landscape at all; it is Swampland. No
//!   amount of capability rescues it, which is why `Theta` multiplies the whole
//!   functional rather than being one more term inside it.
//! * **`B(D)`** — a graded pressure over the constraints that *are* met, or that
//!   are soft. `B = max_j (r_j - a_j)/(r_j + eps)`, clamped to `[0, 1]`.
//!
//! ## The `max`, not the sum, is the point
//!
//! Take a design needing 100 MW with 20 MW available, 10% short on GPUs and 5%
//! short on cooling. Summing gives `0.8 + 0.1 + 0.05 = 0.95` and invites you to
//! believe that fixing cooling helps. It does not. The `max` returns `0.8` and
//! names `power`, because power alone decides the outcome: until the grid
//! connection lands, a perfect cooling loop changes nothing you can measure.
//! This is Amdahl's law wearing work boots.
//!
//! [`Bottleneck::binding`] is therefore the most operationally useful thing in
//! this crate. `K_R` is a score; the binding constraint is an instruction.

use flux_kgauge::provenance::Provenance;
use serde::{Deserialize, Serialize};

/// Guard against dividing by a zero requirement.
const EPS: f64 = 1e-9;

/// Whether failing this constraint makes the design impossible or merely slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hardness {
    /// Violating it puts the design in the Swampland: `Theta_phys = 0`.
    /// Reserve this for constraints no amount of money or time relaxes at the
    /// moment of evaluation — you cannot draw 100 MW from a 20 MW feeder, and
    /// you cannot represent 1 SIGIL in a note whose range proof tops out below
    /// it.
    Hard,
    /// Violating it costs throughput, not existence. It contributes to `B(D)`
    /// and can be the binding constraint, but it never zeroes the functional.
    Soft,
}

/// One resource requirement and what is actually available for it.
///
/// Units are the caller's business and must simply be consistent within a
/// constraint — the ratio is dimensionless, so watts against watts and blocks
/// against blocks both work, but watts against blocks is nonsense the type
/// system cannot catch for you.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    /// Short stable name. This is what gets reported as the binding constraint,
    /// so make it something an operator can act on: `power`, `peers`,
    /// `producer_liveness` — not `constraint_3`.
    pub name: String,
    /// How much the design needs.
    pub required: f64,
    /// How much exists.
    pub available: f64,
    pub hardness: Hardness,
    /// Where `available` came from. A constraint checked against a placeholder
    /// is a model, not a measurement, and the report says so.
    pub provenance: Provenance,
    /// One sentence for a human: what this constraint is and what relaxes it.
    pub note: String,
}

impl Constraint {
    pub fn hard(name: &str, required: f64, available: f64, provenance: Provenance, note: &str) -> Self {
        Self { name: name.to_string(), required, available, hardness: Hardness::Hard, provenance, note: note.to_string() }
    }

    pub fn soft(name: &str, required: f64, available: f64, provenance: Provenance, note: &str) -> Self {
        Self { name: name.to_string(), required, available, hardness: Hardness::Soft, provenance, note: note.to_string() }
    }

    /// Shortfall as a fraction of what is required, clamped to `[0, 1]`.
    ///
    /// Surplus is deliberately NOT negative pressure: having twice the power you
    /// need does not offset being short of GPUs, and letting it do so
    /// arithmetically is how a spreadsheet talks you into a build that cannot
    /// run.
    pub fn pressure(&self) -> f64 {
        if !self.required.is_finite() || !self.available.is_finite() {
            return 0.0;
        }
        ((self.required - self.available) / (self.required.abs() + EPS)).clamp(0.0, 1.0)
    }

    pub fn is_satisfied(&self) -> bool {
        self.available >= self.required
    }
}

/// Result of applying the physical-feasibility gate to a constraint set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feasibility {
    /// `1.0` when every hard constraint is met, `0.0` otherwise. It is a factor,
    /// not a probability.
    pub theta: f64,
    /// `true` = the design is in the Landscape (physically realizable).
    /// `false` = Swampland: mathematically describable, physically not buildable.
    pub in_landscape: bool,
    /// Names of the hard constraints that failed, in declaration order.
    pub violated_hard: Vec<String>,
    /// Worst provenance across the hard constraints — the trust you may place
    /// in the verdict itself.
    pub provenance: Provenance,
}

/// Apply `Theta_phys`: does this design fall in the Landscape or the Swampland?
pub fn feasibility(constraints: &[Constraint]) -> Feasibility {
    let hard: Vec<&Constraint> = constraints.iter().filter(|c| c.hardness == Hardness::Hard).collect();
    let violated: Vec<String> = hard.iter().filter(|c| !c.is_satisfied()).map(|c| c.name.clone()).collect();
    let provenance = if hard.is_empty() {
        // No hard constraints were supplied at all. That is not a clean bill of
        // health, it is an unexamined design — say so rather than returning a
        // confident 1.0 with `Measured` behind it.
        Provenance::Unavailable
    } else {
        Provenance::worst_of(&hard.iter().map(|c| c.provenance).collect::<Vec<_>>())
    };
    Feasibility {
        theta: if violated.is_empty() { 1.0 } else { 0.0 },
        in_landscape: violated.is_empty(),
        violated_hard: violated,
        provenance,
    }
}

/// The binding constraint and how hard it binds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bottleneck {
    /// `B(D) = max_j (r_j - a_j)/(r_j + eps)`, in `[0, 1]`.
    pub pressure: f64,
    /// The constraint achieving that max — the one thing to fix. `None` only
    /// when no constraints were supplied.
    pub binding: Option<String>,
    /// Human sentence from the binding constraint, so a report can explain
    /// itself without the caller re-looking-up the constraint list.
    pub binding_note: Option<String>,
    /// Every constraint under any pressure at all, worst first. The tail is
    /// what you fix *after* the binding one, and it is worth showing because
    /// relieving the binding constraint promotes the next.
    pub ranked: Vec<(String, f64)>,
    pub provenance: Provenance,
}

/// Compute `B(D)` and name the constraint that binds.
pub fn bottleneck(constraints: &[Constraint]) -> Bottleneck {
    let mut ranked: Vec<(String, f64)> =
        constraints.iter().map(|c| (c.name.clone(), c.pressure())).filter(|(_, p)| *p > 0.0).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let worst = constraints
        .iter()
        .max_by(|a, b| a.pressure().partial_cmp(&b.pressure()).unwrap_or(std::cmp::Ordering::Equal));

    match worst {
        Some(c) => Bottleneck {
            pressure: c.pressure(),
            binding: Some(c.name.clone()),
            binding_note: Some(c.note.clone()),
            ranked,
            provenance: Provenance::worst_of(&constraints.iter().map(|x| x.provenance).collect::<Vec<_>>()),
        },
        None => Bottleneck {
            pressure: 0.0,
            binding: None,
            binding_note: None,
            ranked,
            provenance: Provenance::Unavailable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, r: f64, a: f64) -> Constraint {
        Constraint::soft(name, r, a, Provenance::Measured, "test")
    }

    #[test]
    fn the_worked_example_from_the_derivation() {
        // 100 MW needed / 20 MW available, GPUs 10% short, cooling 5% short.
        let cs = vec![p("power", 100.0, 20.0), p("gpu", 100.0, 90.0), p("cooling", 100.0, 95.0)];
        let b = bottleneck(&cs);
        assert!((b.pressure - 0.8).abs() < 1e-9, "B = 0.8, got {}", b.pressure);
        assert_eq!(b.binding.as_deref(), Some("power"));
        // The other two are visible but subordinate — and in the right order.
        assert_eq!(b.ranked.len(), 3);
        assert_eq!(b.ranked[0].0, "power");
        assert_eq!(b.ranked[1].0, "gpu");
    }

    #[test]
    fn surplus_never_becomes_negative_pressure() {
        // Ten times the power you need does not offset a GPU shortfall.
        let cs = vec![p("power", 10.0, 100.0), p("gpu", 100.0, 50.0)];
        let b = bottleneck(&cs);
        assert!((b.pressure - 0.5).abs() < 1e-9);
        assert_eq!(b.binding.as_deref(), Some("gpu"));
        assert_eq!(b.ranked.len(), 1, "a satisfied constraint contributes no pressure");
    }

    #[test]
    fn max_not_sum_is_what_makes_the_answer_actionable() {
        let cs = vec![p("power", 100.0, 20.0), p("gpu", 100.0, 90.0), p("cooling", 100.0, 95.0)];
        let sum: f64 = cs.iter().map(|c| c.pressure()).sum();
        let b = bottleneck(&cs);
        assert!(sum > 0.9, "the sum would read 0.95 and imply cooling matters");
        assert!(b.pressure < sum, "the max refuses to let three shortfalls pretend to be one big one");
    }

    #[test]
    fn a_failed_hard_constraint_is_swampland() {
        let cs = vec![
            Constraint::hard("grid", 100.0, 20.0, Provenance::Measured, "no connection yet"),
            p("gpu", 100.0, 100.0),
        ];
        let f = feasibility(&cs);
        assert_eq!(f.theta, 0.0);
        assert!(!f.in_landscape);
        assert_eq!(f.violated_hard, vec!["grid".to_string()]);
    }

    #[test]
    fn soft_shortfalls_stay_in_the_landscape() {
        let cs = vec![p("gpu", 100.0, 1.0)];
        let f = feasibility(&cs);
        // No hard constraints at all -> feasible, but the provenance says the
        // verdict is unexamined rather than verified.
        assert!(f.in_landscape);
        assert_eq!(f.provenance, Provenance::Unavailable);
    }

    #[test]
    fn feasibility_is_no_more_trustworthy_than_its_worst_hard_input() {
        let cs = vec![
            Constraint::hard("grid", 1.0, 2.0, Provenance::Measured, ""),
            Constraint::hard("permits", 1.0, 2.0, Provenance::Placeholder, "guessed"),
        ];
        let f = feasibility(&cs);
        assert!(f.in_landscape);
        assert_eq!(f.provenance, Provenance::Placeholder);
    }

    #[test]
    fn pressure_is_bounded_and_survives_a_zero_requirement() {
        let c = p("nothing_needed", 0.0, 0.0);
        assert_eq!(c.pressure(), 0.0);
        let c = p("everything_missing", 5.0, -100.0);
        assert!((c.pressure() - 1.0).abs() < 1e-12, "clamped at 1, not 21");
    }

    #[test]
    fn non_finite_inputs_do_not_poison_the_bottleneck() {
        let c = p("bad", f64::NAN, 1.0);
        assert_eq!(c.pressure(), 0.0);
        let b = bottleneck(&[c, p("real", 10.0, 5.0)]);
        assert_eq!(b.binding.as_deref(), Some("real"));
    }
}
