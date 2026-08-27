//! The gauge itself: run one round, get one honest report.
//!
//! Eq. 25 assembles the pieces:
//!
//! ```text
//!                  K_base
//!     K_enhanced = ------- * (1 + (1 - Omega_node) * w_obs)
//!                  Lambda
//! ```
//!
//! Read the two factors as two different sentences the node can say:
//!
//! * `1/Lambda` — "the tip I am measuring is not confirmed yet, so discount my
//!   optimism."
//! * `(1 + (1 - Omega))` — "I cannot see enough of the network to trust my own
//!   reading."
//!
//! When both are near their good values the enhancement is transparent and
//! `K_enhanced ~ K_base`. The paper's own recommendation stands: the base gauge
//! is the operational tool, and the enhancement is a proposed upgrade whose
//! terms each import at least one un-measured quantity. That is why
//! [`crate::config::PhaseDriver::Base`] is the default.

use serde::{Deserialize, Serialize};

use crate::anticone::{self, AnticoneMeasure};
use crate::base::{self, BaseGauge, Confidence};
use crate::commitment::{self, Commitment, CommitmentBasis};
use crate::config::{GaugeConfig, PhaseDriver};
use crate::observables::Observables;
use crate::observer::{self, ObserverCoverage};
use crate::phase::{Phase, PhaseTracker, Tuning};
use crate::provenance::{Note, Provenance};
use std::borrow::Cow;

/// A single caveat attached to a reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caveat {
    /// Machine-stable identifier, for alert routing and dedup.
    pub code: Note,
    /// Plain sentence for a human.
    pub detail: String,
}

impl Caveat {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code: Cow::Borrowed(code), detail: detail.into() }
    }
}

/// Everything one gauge round produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GaugeReport {
    pub base: BaseGauge,
    pub observer: ObserverCoverage,
    pub commitment: Commitment,
    pub anticone: AnticoneMeasure,
    /// `K_enhanced` from Eq. 25, capped at `K_base / lambda_min`.
    pub k_enhanced: f64,
    /// Which value drove the phase decision.
    pub phase_driver: PhaseDriver,
    pub phase: Phase,
    pub previous_phase: Phase,
    pub tuning: Tuning,
    /// Worst provenance anywhere in the chain of computation that produced the
    /// value the phase was decided on.
    pub provenance: Provenance,
    /// Everything a reader must know before believing the numbers above.
    pub caveats: Vec<Caveat>,
}

impl GaugeReport {
    /// The value the phase decision used.
    pub fn driving_value(&self) -> f64 {
        match self.phase_driver {
            PhaseDriver::Base => self.base.value_or_zero(),
            PhaseDriver::Enhanced => self.k_enhanced,
        }
    }

    /// True when this reading is safe to alert on without a human reading the
    /// caveats first.
    ///
    /// Three conditions, all necessary:
    ///
    /// 1. at least one stress channel reported (`Confidence`),
    /// 2. no `Placeholder` input reached the driving value (`Provenance`),
    /// 3. the driving value is not structurally pinned. Specifically: when the
    ///    phase decision runs off `K_enhanced` and `K_base` is exactly zero,
    ///    Eq. 25 multiplies zero by everything and the reading cannot respond
    ///    to *any* degradation. A number that cannot move is not actionable,
    ///    however trustworthy its inputs.
    pub fn is_actionable(&self) -> bool {
        if !self.base.confidence.is_actionable() || !self.provenance.is_operational() {
            return false;
        }
        if self.phase_driver == PhaseDriver::Enhanced
            && self.caveats.iter().any(|c| c.code == "enhancement_inert")
        {
            return false;
        }
        true
    }

    /// Ratio by which the enhancement inflated the base reading.
    pub fn enhancement_ratio(&self) -> f64 {
        let b = self.base.value_or_zero();
        if b.abs() < f64::EPSILON {
            1.0
        } else {
            self.k_enhanced / b
        }
    }

    /// Prometheus text exposition.
    pub fn prometheus(&self, prefix: &str) -> String {
        let mut s = String::new();
        let mut g = |name: &str, help: &str, v: f64| {
            s.push_str(&format!(
                "# HELP {p}_{n} {h}\n# TYPE {p}_{n} gauge\n{p}_{n} {v}\n",
                p = prefix,
                n = name,
                h = help,
                v = v
            ));
        };
        g("k_base", "Base K-gauge (Eq. 10)", self.base.value_or_zero());
        g("k_enhanced", "Enhanced K-gauge (Eq. 25)", self.k_enhanced);
        g("omega_node", "Observer coverage factor (Eq. 17)", self.observer.omega);
        g("lambda_commit", "Commitment irreversibility (Eq. 20)", self.commitment.lambda);
        g("d_commit", "Commitment depth (Eq. 19)", self.commitment.d_commit as f64);
        g(
            "f_irrev",
            "Irreversibility fraction (Eq. 23); -1 when the window is too short to answer",
            self.commitment.f_irrev.unwrap_or(-1.0),
        );
        g("anticone_k_max", "Max observed DAG-Knight anticone width (NOT the K-gauge)", self.anticone.k_observed_max as f64);
        g("distinct_producers", "Distinct block producers in the window", self.anticone.distinct_producers as f64);
        g("phase", "0 stable, 1 approaching, 2 critical", self.phase as u8 as f64);
        g(
            "actionable",
            "1 when the reading is complete and free of placeholder inputs",
            if self.is_actionable() { 1.0 } else { 0.0 },
        );
        g("caveats", "Number of caveats attached to this reading", self.caveats.len() as f64);
        s
    }

    /// Human-readable report that leads with what the numbers are worth.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "K-gauge: K_base = {:.4}  K_enhanced = {:.4}  phase = {} (driven by {:?})\n",
            self.base.value_or_zero(),
            self.k_enhanced,
            self.phase,
            self.phase_driver
        ));
        out.push_str(&format!(
            "  trust: confidence = {:?}, worst provenance = {} ({}), actionable = {}\n",
            self.base.confidence,
            self.provenance,
            self.provenance.code(),
            self.is_actionable()
        ));
        out.push_str(&format!(
            "  dH = {}  ds = {}  tau = {:.1}s\n",
            fmt_opt(self.base.delta_h.total()),
            fmt_opt(self.base.delta_s.total()),
            self.base.tau_secs
        ));
        out.push_str(&format!(
            "  Omega = {:.4} ({} peers / n_total {:.0}{})  Lambda = {:.4} (d_commit {})  f_irrev = {}\n",
            self.observer.omega,
            self.observer.n_peers,
            self.observer.n_total,
            if self.observer.n_total_is_guess { ", GUESSED" } else { "" },
            self.commitment.lambda,
            self.commitment.d_commit,
            fmt_opt(self.commitment.f_irrev)
        ));
        out.push_str(&format!("  DAG: {}\n", self.anticone.summary()));
        if self.caveats.is_empty() {
            out.push_str("  caveats: none\n");
        } else {
            out.push_str("  caveats:\n");
            for c in &self.caveats {
                out.push_str(&format!("    [{}] {}\n", c.code, c.detail));
            }
        }
        out
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => "unavailable".to_string(),
    }
}

/// A gauge instance: config plus rolling history.
#[derive(Debug, Clone)]
pub struct KGauge {
    cfg: GaugeConfig,
    basis: CommitmentBasis,
    tracker: PhaseTracker,
}

impl Default for KGauge {
    fn default() -> Self {
        Self::new(GaugeConfig::default())
    }
}

impl KGauge {
    pub fn new(cfg: GaugeConfig) -> Self {
        Self { cfg, basis: CommitmentBasis::default(), tracker: PhaseTracker::default() }
    }

    pub fn with_commitment_basis(mut self, basis: CommitmentBasis) -> Self {
        self.basis = basis;
        self
    }

    pub fn config(&self) -> &GaugeConfig {
        &self.cfg
    }

    pub fn tracker(&self) -> &PhaseTracker {
        &self.tracker
    }

    /// Run one round.
    pub fn observe(&mut self, obs: &Observables) -> GaugeReport {
        let base = base::compute_base(obs, &self.cfg);
        let observer = observer::compute(obs.current.peer_count, obs.network_size, self.cfg.w_obs);
        let commitment = commitment::compute(obs, &self.cfg, self.basis);
        let anticone = anticone::measure(&obs.window, self.cfg.kappa);

        // Eq. 25, with the documented clamp.
        let k_base = base.value_or_zero();
        let raw = k_base * commitment.commitment_multiplier * observer.correction;
        let cap = k_base * (1.0 / self.cfg.lambda_commit_min.clamp(1e-9, 1.0));
        let k_enhanced = if raw.is_finite() { raw.min(cap) } else { k_base };

        let driving = match self.cfg.phase_driver {
            PhaseDriver::Base => k_base,
            PhaseDriver::Enhanced => k_enhanced,
        };
        let (phase, previous_phase) = self.tracker.observe(driving, &self.cfg.thresholds);

        // Provenance of the value we actually decided on.
        let provenance = match self.cfg.phase_driver {
            PhaseDriver::Base => base.provenance,
            PhaseDriver::Enhanced => Provenance::worst_of(&[
                base.provenance,
                observer.provenance,
                commitment.provenance,
            ]),
        };

        let caveats = collect_caveats(obs, &base, &observer, &commitment, &anticone, &self.cfg);

        GaugeReport {
            base,
            observer,
            commitment,
            anticone,
            k_enhanced,
            phase_driver: self.cfg.phase_driver,
            phase,
            previous_phase,
            tuning: Tuning::for_phase(phase),
            provenance,
            caveats,
        }
    }
}

fn collect_caveats(
    obs: &Observables,
    base: &BaseGauge,
    observer: &ObserverCoverage,
    commitment: &Commitment,
    anticone: &AnticoneMeasure,
    cfg: &GaugeConfig,
) -> Vec<Caveat> {
    let mut c = Vec::new();

    if base.confidence == Confidence::NoEvidence {
        c.push(Caveat::new(
            "no_evidence",
            "nothing moved during the window: no shares, no bytes, no blocks. K = 0 here means 'no data', not 'healthy'.",
        ));
    }

    let missing: Vec<String> = base
        .delta_h
        .missing_channels()
        .into_iter()
        .chain(base.delta_s.missing_channels())
        .collect();
    if !missing.is_empty() {
        c.push(Caveat::new(
            "partial_channels",
            format!(
                "{} of {} stress channels had no data ({}). K is a lower bound: the missing channels could only add stress.",
                missing.len(),
                base.delta_h.channels.len() + base.delta_s.channels.len(),
                missing.join(", ")
            ),
        ));
    }

    if observer.n_total_is_guess {
        c.push(Caveat::new(
            "n_total_hardcoded",
            format!(
                "Omega_node used a hardcoded n_total = {:.0} (paper L11). With {} peers this reports Omega = {:.3}; if the real network is smaller, coverage is better than shown and the observer correction is inflating K by {:.2}x for no reason.",
                observer.n_total, observer.n_peers, observer.omega, observer.correction
            ),
        ));
    }

    if commitment.lambda_is_window_limited() {
        c.push(Caveat::new(
            "lambda_window_limited",
            format!(
                "Lambda_commit = {:.4} is capped by the measurement window (ceiling {:.4}), not by chain health. It will sit near this value permanently and inflate K_enhanced by ~{:.1}x regardless of how settled the chain is. Use CommitmentBasis::ReferenceDepth for a chain-health reading.",
                commitment.lambda, commitment.lambda_ceiling, commitment.commitment_multiplier
            ),
        ));
    }

    if commitment.f_irrev.is_none() {
        c.push(Caveat::new(
            "f_irrev_unavailable",
            format!(
                "f_irrev not reported: {}. Window holds {} blocks, D_reorg = {}.",
                commitment.f_irrev_note, commitment.window_blocks, commitment.reorg_depth_bound
            ),
        ));
    }

    // Eq. 25 multiplies K_base by both correction factors. Multiplication by
    // zero is zero: if the base gauge reads exactly 0 — which is the *normal*
    // reading for a quiet, in-sync node — then no amount of observer or
    // commitment degradation can move K_enhanced off zero. The paper's Table 6
    // never exercises this case; every row starts from K_base = 0.19 or 1.27.
    // So the headline claim that the enhancement "catches a Sybil partition"
    // holds only where the base gauge is already non-zero.
    if base.value_or_zero().abs() < 1e-12 {
        let degraded = observer.omega < 0.5 || commitment.lambda < 0.5;
        if degraded {
            c.push(Caveat::new(
                "enhancement_inert",
                format!(
                    "K_base is exactly 0, so Eq. 25 yields K_enhanced = 0 no matter how bad coverage or commitment get (Omega = {:.3}, Lambda = {:.3}). The enhancement is multiplicative and cannot flag a partition on an otherwise-quiet node. Read Omega and Lambda directly instead of relying on K_enhanced here.",
                    observer.omega, commitment.lambda
                ),
            ));
        }
    }

    if anticone.dag_metrics_are_vacuous() {
        c.push(Caveat::new(
            "dag_vacuous",
            format!(
                "{} — DAG-shaped health signals carry no information in this window.",
                anticone.summary()
            ),
        ));
    }

    if anticone.colouring_inconsistencies > 0 {
        c.push(Caveat::new(
            "colouring_inconsistent",
            format!(
                "{} blocks report is_blue = false while their blue_score advances. The chain's own colouring is self-contradictory; anything derived from it is suspect.",
                anticone.colouring_inconsistencies
            ),
        ));
    }

    if let Some(rate) = obs.block_rate() {
        let target = cfg.target_block_rate_bps;
        if target > 0.0 && rate > 0.0 {
            let ratio = rate / target;
            if !(0.2..=5.0).contains(&ratio) {
                c.push(Caveat::new(
                    "target_rate_mismatch",
                    format!(
                        "observed {rate:.2} blocks/s against a configured target of {target:.2} blocks/s ({ratio:.1}x). If the chain is healthy, the target is wrong and the block-rate channel is manufacturing stress."
                    ),
                ));
            }
        }
    }

    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observables::{BlockFact, BlockWindow, CounterSample, NetworkSize};

    fn chain_window(n: u64, producers: u64) -> BlockWindow {
        BlockWindow::new(
            (0..n)
                .map(|i| BlockFact {
                    height: 1_000_000 + i,
                    producer: [(i % producers.max(1)) as u8; 32],
                    merge_parent_count: 0,
                    blue_score: Some(i),
                    is_blue: Some(true),
                })
                .collect(),
        )
    }

    fn healthy_obs() -> Observables {
        Observables {
            previous: CounterSample {
                mining_submitted: 1_000,
                mining_accepted: 990,
                p2p_bytes_in: 10_000,
                p2p_bytes_out: 10_000,
                peer_count: 12,
                local_height: 2_000_000,
                network_height: 2_000_000,
            },
            current: CounterSample {
                mining_submitted: 1_100,
                mining_accepted: 1_089,
                p2p_bytes_in: 20_000,
                p2p_bytes_out: 20_000,
                peer_count: 12,
                local_height: 2_000_060,
                network_height: 2_000_060,
            },
            window_secs: 60.0,
            window: chain_window(60, 4),
            network_size: NetworkSize::Known(12),
            kappa: 18.0,
        }
    }

    #[test]
    fn healthy_network_is_stable_and_actionable() {
        let mut g = KGauge::new(GaugeConfig::quillon());
        let r = g.observe(&healthy_obs());
        assert_eq!(r.phase, Phase::Stable);
        assert!(r.base.value_or_zero() < 1.0);
        assert!(r.is_actionable(), "report: {}", r.render());
    }

    /// A node carrying a little baseline stress, so `K_base > 0` and the
    /// multiplicative enhancement has something to act on.
    fn slightly_stressed_obs() -> Observables {
        let mut o = healthy_obs();
        // A small, steady height gap from the network: ds becomes non-zero.
        o.current.network_height = o.current.local_height + 30;
        o
    }

    #[test]
    fn sybil_partition_is_invisible_to_base_but_not_to_enhanced() {
        let mut sybil = slightly_stressed_obs();
        sybil.current.peer_count = 2;
        sybil.previous.peer_count = 2;

        let mut g = KGauge::new(
            GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced),
        );
        let healthy = g.observe(&slightly_stressed_obs());
        let attacked = g.observe(&sybil);

        // Base is untouched: every counter the base gauge reads is identical.
        let base_delta = (attacked.base.value_or_zero() - healthy.base.value_or_zero()).abs();
        assert!(base_delta < 1e-9, "base moved by {base_delta}");
        assert!(healthy.base.value_or_zero() > 0.0, "test needs a non-zero base");

        // The observer term does move, and it moves K_enhanced with it.
        assert!(attacked.observer.omega < healthy.observer.omega);
        assert!(attacked.observer.correction > healthy.observer.correction);
        assert!(
            attacked.k_enhanced > healthy.k_enhanced,
            "attacked {} vs healthy {}",
            attacked.k_enhanced,
            healthy.k_enhanced
        );
    }

    #[test]
    fn an_inert_enhanced_reading_is_not_actionable() {
        let mut sybil = healthy_obs();
        sybil.current.peer_count = 1;
        sybil.previous.peer_count = 1;

        let mut enh = KGauge::new(GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced));
        let r = enh.observe(&sybil);
        assert!(!r.is_actionable(), "a pinned K_enhanced must not read as actionable");

        // The base driver on the same data is still actionable: K_base = 0 is a
        // real statement there, it just is not sensitive to coverage.
        let mut b = KGauge::new(GaugeConfig::quillon());
        assert!(b.observe(&sybil).is_actionable());
    }

    #[test]
    fn enhancement_is_inert_when_the_base_gauge_reads_exactly_zero() {
        // The failure mode the paper's Table 6 never exercises: every row there
        // starts from a non-zero K_base. Eq. 25 is a product, so a quiet,
        // perfectly in-sync node reads K_base = 0 and K_enhanced = 0 even under
        // a total coverage collapse.
        let mut sybil = healthy_obs(); // K_base is exactly 0 here
        sybil.current.peer_count = 1;
        sybil.previous.peer_count = 1;

        let mut g = KGauge::new(
            GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced),
        );
        let r = g.observe(&sybil);

        assert!(r.base.value_or_zero().abs() < 1e-12);
        assert!(r.observer.omega < 0.1, "omega {}", r.observer.omega);
        assert!(
            r.k_enhanced.abs() < 1e-12,
            "K_enhanced should be pinned to 0, got {}",
            r.k_enhanced
        );
        assert_eq!(r.phase, Phase::Stable);
        // The gauge says so out loud instead of quietly reporting health.
        assert!(
            r.caveats.iter().any(|c| c.code == "enhancement_inert"),
            "caveats: {:?}",
            r.caveats
        );
    }

    #[test]
    fn fresh_restart_flags_a_shallow_tip() {
        let mut fresh = healthy_obs();
        fresh.current.local_height = fresh.previous.local_height + 5;
        fresh.window = chain_window(5, 1);

        let mut g = KGauge::new(GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced));
        let r = g.observe(&fresh);
        assert!(r.commitment.lambda < 0.01, "lambda {}", r.commitment.lambda);
        assert!(r.enhancement_ratio() > 50.0, "ratio {}", r.enhancement_ratio());
        assert!(r.caveats.iter().any(|c| c.code == "lambda_window_limited"));
    }

    #[test]
    fn enhanced_never_exceeds_the_documented_cap() {
        let mut worst = healthy_obs();
        worst.current.peer_count = 1;
        worst.current.local_height = worst.previous.local_height + 1;
        worst.current.mining_accepted = worst.previous.mining_accepted; // all rejected
        worst.network_size = NetworkSize::Placeholder(100_000);

        let mut g = KGauge::new(GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced));
        let r = g.observe(&worst);
        let ratio = r.enhancement_ratio();
        assert!(ratio <= 100.0 + 1e-9, "ratio {ratio} exceeded the 100x cap");
    }

    #[test]
    fn idle_node_is_not_reported_as_healthy() {
        let mut g = KGauge::default();
        let r = g.observe(&Observables::default());
        assert_eq!(r.base.confidence, Confidence::NoEvidence);
        assert!(!r.is_actionable());
        assert!(r.caveats.iter().any(|c| c.code == "no_evidence"));
    }

    #[test]
    fn hardcoded_n_total_makes_enhanced_driver_non_actionable() {
        let mut o = healthy_obs();
        o.network_size = NetworkSize::Placeholder(50);
        let mut g = KGauge::new(GaugeConfig::quillon().with_phase_driver(PhaseDriver::Enhanced));
        let r = g.observe(&o);
        assert_eq!(r.provenance, Provenance::Placeholder);
        assert!(!r.is_actionable());
        assert!(r.caveats.iter().any(|c| c.code == "n_total_hardcoded"));

        // ...while the base driver on the same data stays actionable.
        let mut gb = KGauge::new(GaugeConfig::quillon());
        let rb = gb.observe(&o);
        assert!(rb.is_actionable());
    }

    #[test]
    fn straight_chain_raises_the_vacuous_dag_caveat() {
        let mut o = healthy_obs();
        o.window = chain_window(60, 1); // one producer, no merges
        let mut g = KGauge::new(GaugeConfig::quillon());
        let r = g.observe(&o);
        assert!(r.caveats.iter().any(|c| c.code == "dag_vacuous"));
        assert_eq!(r.anticone.k_observed_max, 0);
    }

    #[test]
    fn wrong_target_block_rate_is_called_out() {
        // This is the check that caught the stale 6.28 bps SIGIL figure against
        // a live 0.83 bps measurement.
        let mut o = healthy_obs();
        o.current.local_height = o.previous.local_height + 50; // 0.83 bps
        o.current.mining_accepted = o.previous.mining_accepted + 90; // some stress in dH

        let mut wrong = KGauge::new(GaugeConfig::sigil().with_target_block_rate(6.28));
        let r = wrong.observe(&o);
        assert!(
            r.caveats.iter().any(|c| c.code == "target_rate_mismatch"),
            "caveats: {:?}",
            r.caveats
        );

        let mut right = KGauge::new(GaugeConfig::sigil());
        let r2 = right.observe(&o);
        assert!(!r2.caveats.iter().any(|c| c.code == "target_rate_mismatch"));
        // And the phantom stress goes away with the correct target.
        assert!(
            r2.base.value_or_zero() < r.base.value_or_zero(),
            "corrected {} should be below miscalibrated {}",
            r2.base.value_or_zero(),
            r.base.value_or_zero()
        );
    }

    #[test]
    fn prometheus_output_is_well_formed() {
        let mut g = KGauge::default();
        let r = g.observe(&healthy_obs());
        let p = r.prometheus("qnk");
        assert!(p.contains("# TYPE qnk_k_base gauge"));
        assert!(p.contains("qnk_anticone_k_max"));
        assert!(p.contains("qnk_actionable"));
        // Every line is either a comment or `name value`.
        for line in p.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts.len(), 2, "bad metric line: {line}");
            assert!(parts[1].parse::<f64>().is_ok(), "unparseable value in: {line}");
        }
    }

    #[test]
    fn render_leads_with_trust() {
        let mut g = KGauge::default();
        let r = g.observe(&Observables::default());
        let s = r.render();
        assert!(s.contains("trust:"));
        assert!(s.contains("no_evidence"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut g = KGauge::default();
        let r = g.observe(&healthy_obs());
        let js = serde_json::to_string(&r).expect("serialize");
        let back: GaugeReport = serde_json::from_str(&js).expect("deserialize");

        // Decimal JSON does not preserve every f64 bit pattern, so compare on
        // meaning rather than on `==`.
        assert!((back.base.value_or_zero() - r.base.value_or_zero()).abs() < 1e-12);
        assert!((back.k_enhanced - r.k_enhanced).abs() < 1e-12);
        assert!((back.observer.omega - r.observer.omega).abs() < 1e-12);
        assert!((back.commitment.lambda - r.commitment.lambda).abs() < 1e-12);
        assert_eq!(back.phase, r.phase);
        assert_eq!(back.commitment.d_commit, r.commitment.d_commit);
        assert_eq!(back.commitment.f_irrev.is_none(), r.commitment.f_irrev.is_none());
        assert_eq!(back.anticone.shape, r.anticone.shape);
        assert_eq!(
            back.caveats.iter().map(|c| c.code.to_string()).collect::<Vec<_>>(),
            r.caveats.iter().map(|c| c.code.to_string()).collect::<Vec<_>>()
        );
        // The notes survive as owned strings.
        assert_eq!(back.base.delta_h.channels[0].name, r.base.delta_h.channels[0].name);
    }
}
