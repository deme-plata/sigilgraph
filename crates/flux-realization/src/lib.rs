//! # `flux-realization` — the Kristensen Realization Functional `K_R`
//!
//! ## What this is, in one paragraph
//!
//! `flux-kgauge` answers *"how stressed is this consensus right now?"*. This
//! crate answers a different and, for an engineer, more useful question:
//! **"of everything this system could be, which configuration is physically
//! realizable, how good is it, and what single thing is stopping it?"**
//!
//! The framing is lifted directly from the Landscape/Swampland distinction in
//! *Beyond the Horizon*: an enormous space of mathematically describable
//! configurations, only a subset of which any physics permits. Applied to
//! engineering, the claim is deflationary and useful — the design you want does
//! not exist in some other branch waiting to be fetched; it exists as a point in
//! a **search space you can explore computationally without building anything**,
//! and the whole job is to find it, prove it feasible, and then cause it.
//!
//! ```text
//!                  K_R(D) = Theta_phys(D) * [ C E V A X ]^(1/5) * ln(1 + N_p^eff)
//!                           -----------------------------------------------------
//!                                        1 + K_coord(D) + B(D)
//! ```
//!
//! * `Theta_phys` — the feasibility gate. `1` in the Landscape, `0` in the
//!   Swampland. It multiplies, so an infeasible design scores exactly zero no
//!   matter how attractive the numerator. See [`constraints::feasibility`].
//! * `[C E V A X]^(1/5)` — the capability core: compute, energy, verifiability,
//!   availability, expandability, as a **geometric mean over the terms actually
//!   measured**. See [`capability`].
//! * `ln(1 + N_p^eff)` — peer amplification, where `N_p^eff = sum u_i v_i r_i`
//!   weights each peer by useful work x verification x reliability, so a Sybil
//!   fleet contributes nothing. See [`peers`].
//! * `K_coord` — coordination stress. On SIGIL this is fed the dimensionless
//!   `K*` from `flux-kgauge`, so the two gauges compose instead of competing.
//! * `B` — bottleneck pressure, `max_j (r_j - a_j)/r_j`, which **names the
//!   binding constraint**. See [`constraints::bottleneck`].
//!
//! ## Replication beats monumentality — and the functional says so
//!
//! [`Velocity`] is the part with teeth. One perfect datacenter is not the goal;
//! aggregate realized capability is. `Gamma_R = N_p^eff * K_R / T_realize`
//! makes the trade explicit, and the answer is frequently counterintuitive: a
//! 50 MW modular design replicable by 500 clusters beats a 1 GW monument that
//! only one site on Earth can build. [`tests::replication_beats_monumentality`]
//! runs exactly that comparison with the numbers from the derivation.
//!
//! ## What is honest here, and what is not
//!
//! **Real:** every formula, every bound, every test. The arithmetic is
//! self-contained and does what the doc comments say.
//!
//! **Not a law of nature:** `K_R` is an *engineering figure of merit*, chosen —
//! the exponent `1/5`, the logarithm, the `1 +` in the denominator. It is
//! falsifiable as a ranking (does it order designs the way outcomes do?) and it
//! is not derived from anything more fundamental. Do not let the notation imply
//! otherwise.
//!
//! **Genuinely unavailable:** the `energy` term cannot be measured from a SIGIL
//! node — there is no power telemetry — so on live readings it is *excluded from
//! the mean and named in `excluded`*, never substituted. See [`capability`].

pub mod capability;
pub mod constraints;
pub mod peers;

pub use capability::{Capability, CapabilityCore};
pub use constraints::{bottleneck, feasibility, Bottleneck, Constraint, Feasibility, Hardness};
pub use flux_kgauge::provenance::{Provenance, Tracked};
pub use peers::{amplification, Amplification, Peer};

use serde::{Deserialize, Serialize};

/// Everything needed to evaluate one candidate design `D`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Design {
    /// Short name, so a comparison of several designs is readable.
    pub name: String,
    pub capability: Capability,
    /// Every resource requirement, hard and soft. Drives both `Theta_phys` and
    /// `B(D)`.
    pub constraints: Vec<Constraint>,
    pub peers: Vec<Peer>,
    /// Provenance to attach to the peer set as a whole.
    pub peer_provenance: Provenance,
    /// `K_coord` — coordination stress, `>= 0`. Feed `flux-kgauge`'s `K*` here
    /// on a live chain. Zero means "everything is already coordinated", which is
    /// almost never true and should be justified when claimed.
    pub coordination: Tracked<f64>,
}

/// A complete `K_R` evaluation, with every intermediate exposed. Nothing is
/// hidden behind the headline number — the whole point is that a reader can see
/// which factor produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Realization {
    pub name: String,
    /// The functional itself.
    pub k_r: f64,
    pub feasibility: Feasibility,
    pub capability: CapabilityCore,
    pub amplification: Amplification,
    pub bottleneck: Bottleneck,
    /// `K_coord` as supplied.
    pub coordination: f64,
    /// `1 + K_coord + B` — the denominator, exposed because "what is dividing my
    /// score" is the second question every reader asks.
    pub friction: f64,
    /// Which single factor is costing the most: the plain-language instruction.
    pub limiting_factor: String,
    /// Worst provenance across every input — the trust you may place in `k_r`.
    pub provenance: Provenance,
    /// Sentences a report should print verbatim. Populated whenever a term was
    /// unavailable, guessed, or structurally unreliable.
    pub caveats: Vec<String>,
}

impl Realization {
    /// True when every input was measured, derived or a protocol constant, i.e.
    /// the reading can be acted on without a human first reading the caveats.
    pub fn is_actionable(&self) -> bool {
        self.provenance.is_operational()
    }
}

/// Evaluate `K_R` for one design.
pub fn evaluate(d: &Design) -> Realization {
    let feas = feasibility(&d.constraints);
    let core = d.capability.core();
    let amp = amplification(&d.peers, d.peer_provenance);
    let bn = bottleneck(&d.constraints);

    let k_coord = if d.coordination.value.is_finite() { d.coordination.value.max(0.0) } else { 0.0 };
    let friction = 1.0 + k_coord + bn.pressure;
    let k_r = feas.theta * core.core * amp.p2p / friction;

    // Which factor is actually costing the most? The terms sit in different
    // places in the formula (a multiplier, a logarithm, a divisor), so their
    // magnitudes are not comparable and eyeballing "the smallest one" is wrong.
    //
    // Instead: for each factor, ask what MULTIPLIER on K_R you would get by
    // relieving that factor to its best ATTAINABLE value, and report the largest.
    // "Attainable" is doing real work here — each counterfactual must be bounded
    // and meaningful, or the comparison is rigged:
    //   * capability -> 1.0            (gain = 1/core)
    //   * friction   -> 1.0            (gain = friction; no stress, no bottleneck)
    //   * peers      -> every SUPPLIED peer pulling full weight, i.e.
    //                   N_eff -> raw_peers (gain = ln(1+raw)/ln(1+N_eff))
    // The peer counterfactual is deliberately NOT "ten times the peers": that is
    // unbounded, so it would win almost every comparison and the diagnosis would
    // always read "get more peers" — useless advice on a fleet whose real problem
    // is that the machines it already has are failing verification.
    let limiting_factor = if !feas.in_landscape {
        format!("infeasible — hard constraint(s) violated: {}", feas.violated_hard.join(", "))
    } else if core.core <= 0.0 && !core.zeroed.is_empty() {
        format!("capability term(s) at zero: {}", core.zeroed.join(", "))
    } else if amp.effective_peers <= 0.0 {
        "no effective peers — nothing verifiable is being contributed".to_string()
    } else {
        let ideal_core = if core.core > 0.0 { 1.0 / core.core } else { f64::INFINITY };
        let ideal_friction = friction; // dividing by 1 instead of `friction`
        let ideal_peers = if amp.p2p > 0.0 { (1.0 + amp.raw_peers as f64).ln() / amp.p2p } else { f64::INFINITY };
        let mut best = ("capability", ideal_core);
        if ideal_friction > best.1 {
            best = (if bn.pressure > k_coord { "bottleneck" } else { "coordination" }, ideal_friction);
        }
        if ideal_peers > best.1 {
            best = ("peers", ideal_peers);
        }
        match best.0 {
            "bottleneck" => match (&bn.binding, &bn.binding_note) {
                (Some(n), Some(note)) => format!("bottleneck: {n} — {note}"),
                (Some(n), None) => format!("bottleneck: {n}"),
                _ => "bottleneck".to_string(),
            },
            "coordination" => format!("coordination stress K_coord = {k_coord:.3}"),
            "peers" => "peer amplification — too few effective contributors".to_string(),
            _ => format!(
                "capability core = {:.3} (weakest measured term: {})",
                core.core,
                weakest_term(&d.capability).unwrap_or_else(|| "unknown".to_string())
            ),
        }
    };

    let mut caveats = Vec::new();
    if !core.excluded.is_empty() {
        caveats.push(format!(
            "capability core is a {}-term geometric mean; {} excluded as unavailable and the reading is blind to {}",
            core.included.len(),
            core.excluded.join(", "),
            core.excluded.join(", ")
        ));
    }
    if feas.provenance == Provenance::Unavailable {
        caveats.push("no HARD constraints were supplied — Theta_phys = 1 means 'unexamined', not 'verified feasible'".to_string());
    }
    if d.coordination.provenance != Provenance::Measured {
        caveats.push(format!("K_coord is {} ({})", d.coordination.provenance, d.coordination.note));
    }
    if amp.raw_peers > 0 && amp.quality < 0.25 {
        caveats.push(format!(
            "peer quality {:.0}% — {} peers supplied, {:.2} effective",
            amp.quality * 100.0,
            amp.raw_peers,
            amp.effective_peers
        ));
    }
    caveats.push("N_p^eff enters TWICE across the pair: logarithmically inside K_R (network effect on quality) and linearly in Gamma_R (parallel replication). That is the derivation as written and it makes Gamma_R roughly quadratic in effective peers — read replication comparisons with that in mind".to_string());
    caveats.push("K_R is an engineering figure of merit, not a physical law: the 1/5 exponent, the logarithm and the 1+ in the denominator are chosen, and it is falsifiable only as a ranking".to_string());

    let provenance = Provenance::worst_of(&[
        feas.provenance,
        core.provenance,
        amp.provenance,
        bn.provenance,
        d.coordination.provenance,
    ]);

    Realization {
        name: d.name.clone(),
        k_r,
        feasibility: feas,
        capability: core,
        amplification: amp,
        bottleneck: bn,
        coordination: k_coord,
        friction,
        limiting_factor,
        provenance,
        caveats,
    }
}

fn weakest_term(c: &Capability) -> Option<String> {
    [
        ("compute", &c.compute),
        ("energy", &c.energy),
        ("verifiable", &c.verifiable),
        ("availability", &c.availability),
        ("expandable", &c.expandable),
    ]
    .into_iter()
    .filter(|(_, t)| t.provenance != Provenance::Unavailable)
    .min_by(|a, b| a.1.value.partial_cmp(&b.1.value).unwrap_or(std::cmp::Ordering::Equal))
    .map(|(n, t)| format!("{n} = {:.3}", t.value))
}

/// `Gamma_R` — realization velocity: how much realized capability per unit time
/// a design delivers once replication is accounted for.
///
/// ```text
///   Gamma_R = N_p^eff * K_R / (T_build + T_grid + T_commission)
/// ```
///
/// Units are "realizable design-equivalents per unit of the time inputs" — the
/// caller chooses whether the T's are days or years, and the answer inherits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Velocity {
    pub name: String,
    pub gamma_r: f64,
    pub k_r: f64,
    pub effective_peers: f64,
    pub time_total: f64,
    pub provenance: Provenance,
}

/// Compute `Gamma_R` from an evaluated design and its three time inputs.
///
/// A non-positive total time yields `0.0` rather than an infinity: "it takes no
/// time to build" is a data error, and propagating `inf` through a comparison
/// makes the worst design win.
pub fn velocity(r: &Realization, t_build: f64, t_grid: f64, t_commission: f64) -> Velocity {
    let total = t_build + t_grid + t_commission;
    let gamma = if total > 0.0 && total.is_finite() {
        r.amplification.effective_peers * r.k_r / total
    } else {
        0.0
    };
    Velocity {
        name: r.name.clone(),
        gamma_r: gamma,
        k_r: r.k_r,
        effective_peers: r.amplification.effective_peers,
        time_total: total,
        provenance: r.provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(c: f64, e: f64, v: f64, a: f64, x: f64) -> Capability {
        Capability {
            compute: Tracked::measured(c, "t"),
            energy: Tracked::measured(e, "t"),
            verifiable: Tracked::measured(v, "t"),
            availability: Tracked::measured(a, "t"),
            expandable: Tracked::measured(x, "t"),
        }
    }

    fn good_peers(n: usize) -> Vec<Peer> {
        // `useful` is a per-peer score, NOT a share of the fleet — see peers.rs.
        (0..n).map(|i| Peer::new(&format!("p{i}"), 1.0, 1.0, 1.0)).collect()
    }

    fn design(name: &str, capability: Capability, constraints: Vec<Constraint>, peers: Vec<Peer>) -> Design {
        Design {
            name: name.to_string(),
            capability,
            constraints,
            peers,
            peer_provenance: Provenance::Measured,
            coordination: Tracked::measured(0.0, "test"),
        }
    }

    #[test]
    fn an_infeasible_design_scores_exactly_zero_however_good_it_looks() {
        let d = design(
            "dream",
            cap(1.0, 1.0, 1.0, 1.0, 1.0),
            vec![Constraint::hard("grid", 1000.0, 20.0, Provenance::Measured, "no interconnect")],
            good_peers(1000),
        );
        let r = evaluate(&d);
        assert_eq!(r.k_r, 0.0, "Theta multiplies; it does not merely penalize");
        assert!(r.limiting_factor.contains("infeasible"));
        assert!(r.limiting_factor.contains("grid"));
    }

    #[test]
    fn replication_beats_monumentality() {
        // The comparison from the derivation, made concrete.
        // A: 1 GW monument, one site on Earth can build it, 8 years.
        let a = design(
            "monument-1GW",
            cap(1.0, 0.9, 0.9, 0.9, 0.05), // expandability 0.05: unique site
            vec![Constraint::soft("sites", 1.0, 1.0, Provenance::Measured, "one site exists")],
            good_peers(1),
        );
        // B: 50 MW modular, 500 regional clusters can each build one, 1 year.
        let b = design(
            "modular-50MW",
            cap(0.05, 0.9, 0.9, 0.9, 0.95), // 1/20 the compute, but replicable
            vec![Constraint::soft("sites", 500.0, 500.0, Provenance::Measured, "500 clusters")],
            good_peers(500),
        );
        let (ra, rb) = (evaluate(&a), evaluate(&b));
        let (va, vb) = (velocity(&ra, 8.0, 0.0, 0.0), velocity(&rb, 1.0, 0.0, 0.0));
        assert!(
            vb.gamma_r > va.gamma_r,
            "modular {:.4} should beat monument {:.4}",
            vb.gamma_r,
            va.gamma_r
        );
        // And the monument is not merely slower — it is worse per unit time by
        // orders of magnitude, which is the claim worth defending. The margin is
        // this large partly because N_p^eff enters Gamma_R twice (see the caveat
        // `evaluate` attaches); the ORDERING is the robust result, the exact
        // ratio is model-dependent and should not be quoted as a measurement.
        assert!(vb.gamma_r > va.gamma_r * 100.0, "{:.3} vs {:.3}", vb.gamma_r, va.gamma_r);
    }

    #[test]
    fn the_bottleneck_is_reported_as_an_instruction_not_a_score() {
        let d = design(
            "power-limited",
            cap(0.9, 0.9, 0.9, 0.9, 0.9),
            vec![
                Constraint::soft("power", 100.0, 20.0, Provenance::Measured, "grid connection pending"),
                Constraint::soft("gpu", 100.0, 90.0, Provenance::Measured, "10% short"),
            ],
            good_peers(50),
        );
        let r = evaluate(&d);
        assert_eq!(r.bottleneck.binding.as_deref(), Some("power"));
        assert!(r.limiting_factor.contains("power"), "got: {}", r.limiting_factor);
        assert!(r.limiting_factor.contains("grid connection pending"), "the note travels with it");
    }

    #[test]
    fn relieving_the_binding_constraint_raises_k_r_and_promotes_the_next() {
        let mk = |power_available: f64| {
            design(
                "d",
                cap(0.9, 0.9, 0.9, 0.9, 0.9),
                vec![
                    Constraint::soft("power", 100.0, power_available, Provenance::Measured, "grid"),
                    Constraint::soft("gpu", 100.0, 90.0, Provenance::Measured, "supply"),
                ],
                good_peers(50),
            )
        };
        let before = evaluate(&mk(20.0));
        let after = evaluate(&mk(100.0));
        assert!(after.k_r > before.k_r);
        assert_eq!(after.bottleneck.binding.as_deref(), Some("gpu"), "the next constraint is promoted");
    }

    #[test]
    fn coordination_stress_divides_rather_than_zeroes() {
        let mut d = design("d", cap(0.9, 0.9, 0.9, 0.9, 0.9), vec![], good_peers(10));
        let calm = evaluate(&d);
        d.coordination = Tracked::measured(4.0, "K* from flux-kgauge");
        let stressed = evaluate(&d);
        assert!(stressed.k_r < calm.k_r);
        assert!(stressed.k_r > 0.0, "stress degrades; it does not annihilate");
        assert!((stressed.friction - 5.0).abs() < 1e-12);
    }

    #[test]
    fn an_unavailable_energy_term_produces_a_usable_reading_with_a_caveat() {
        let mut c = cap(0.8, 0.0, 0.8, 0.8, 0.8);
        c.energy = Tracked::unavailable(0.0, "no power telemetry on sigil-api");
        let d = design("sigil-live", c, vec![], good_peers(8));
        let r = evaluate(&d);
        assert!(r.k_r > 0.0);
        assert_eq!(r.capability.excluded, vec!["energy".to_string()]);
        assert!(r.caveats.iter().any(|c| c.contains("energy")));
        assert!(!r.is_actionable(), "a reading blind to a term is not alert-grade");
    }

    #[test]
    fn every_reading_carries_the_not_a_physical_law_caveat() {
        let r = evaluate(&design("d", cap(0.5, 0.5, 0.5, 0.5, 0.5), vec![], good_peers(3)));
        assert!(r.caveats.iter().any(|c| c.contains("not a physical law")));
    }

    #[test]
    fn zero_time_does_not_make_the_worst_design_win() {
        let r = evaluate(&design("d", cap(0.5, 0.5, 0.5, 0.5, 0.5), vec![], good_peers(3)));
        let v = velocity(&r, 0.0, 0.0, 0.0);
        assert_eq!(v.gamma_r, 0.0, "not infinity");
    }

    #[test]
    fn a_reading_serializes_to_json_a_dashboard_can_consume() {
        let r = evaluate(&design("d", cap(0.5, 0.5, 0.5, 0.5, 0.5), vec![], good_peers(3)));
        let j = serde_json::to_value(&r).expect("serializes");
        assert!(j.get("k_r").is_some());
        assert!(j.get("limiting_factor").is_some());
        assert!(j.get("caveats").is_some());
    }
}
