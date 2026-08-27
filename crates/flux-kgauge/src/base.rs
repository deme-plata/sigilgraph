//! The base K-gauge, Eq. 10:
//!
//! ```text
//!         2*pi * sqrt(dH * ds)
//!     K = --------------------
//!                 tau
//! ```
//!
//! `dH` aggregates *operational* stress (is this node's own work going well?),
//! `ds` aggregates *network* stress (is this node in step with everyone else?).
//! The geometric mean is deliberate: it is zero when either side is zero, so
//! you need both a local problem and a network problem to raise an alarm.
//!
//! Two honesty properties this implementation has that a naive one does not:
//!
//! 1. **A channel with no data is `Unavailable`, not `0.0`.** If we never heard
//!    a peer height, "sync divergence 0" would read as *perfectly in sync* when
//!    the truth is *we have no idea*. Those must not look the same.
//! 2. **`K = 0` from an idle node is not `K = 0` from a healthy node.** A node
//!    that submitted no shares, moved no bytes and produced no blocks yields
//!    `dH = ds = 0` and therefore `K = 0` — the best possible reading, produced
//!    by the total absence of evidence. [`Confidence`] separates the two.

use serde::{Deserialize, Serialize};

use crate::config::GaugeConfig;
use crate::observables::Observables;
use crate::provenance::{Note, Provenance, Tracked};
use std::borrow::Cow;

/// One measurable contribution to `dH` or `ds`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub name: Note,
    /// `None` when the inputs for this channel were absent this round.
    pub value: Option<f64>,
    pub provenance: Provenance,
    pub note: Note,
}

impl Channel {
    fn present(name: &'static str, value: f64, note: &'static str) -> Self {
        let value = if value.is_finite() { Some(value.max(0.0)) } else { None };
        Self {
            name: Cow::Borrowed(name),
            value,
            provenance: if value.is_some() { Provenance::Derived } else { Provenance::Unavailable },
            note: Cow::Borrowed(note),
        }
    }

    fn absent(name: &'static str, note: &'static str) -> Self {
        Self { name: Cow::Borrowed(name), value: None, provenance: Provenance::Unavailable, note: Cow::Borrowed(note) }
    }

    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}

/// A sum over channels that remembers which ones were missing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub name: Note,
    pub channels: Vec<Channel>,
}

impl Aggregate {
    pub fn present_channels(&self) -> usize {
        self.channels.iter().filter(|c| c.is_present()).count()
    }

    pub fn missing_channels(&self) -> Vec<String> {
        self.channels
            .iter()
            .filter(|c| !c.is_present())
            .map(|c| c.name.to_string())
            .collect()
    }

    /// Sum of the channels that reported. `None` when nothing reported at all —
    /// which is a different statement from "the sum is zero".
    pub fn total(&self) -> Option<f64> {
        if self.present_channels() == 0 {
            return None;
        }
        Some(self.channels.iter().filter_map(|c| c.value).sum())
    }

    /// Worst provenance among the channels that actually contributed. Missing
    /// channels are reported separately rather than poisoning the sum, because
    /// a partially instrumented aggregate is still useful — it just must not be
    /// mistaken for a complete one.
    pub fn provenance(&self) -> Provenance {
        if self.present_channels() == 0 {
            return Provenance::Unavailable;
        }
        self.channels
            .iter()
            .filter(|c| c.is_present())
            .map(|c| c.provenance)
            .fold(Provenance::Measured, Provenance::worst)
    }

    pub fn is_complete(&self) -> bool {
        self.present_channels() == self.channels.len()
    }
}

/// How much a reading is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Every channel reported. The number means what it says.
    Full,
    /// Some channels reported. The number is a lower bound on stress: the
    /// missing channels can only have added to it.
    Partial,
    /// Nothing moved during the window. `K = 0` here means "no evidence", and
    /// must not be rendered as a clean bill of health.
    NoEvidence,
    /// Neither aggregate could be formed. There is no reading.
    None,
}

impl Confidence {
    pub fn is_actionable(self) -> bool {
        matches!(self, Confidence::Full | Confidence::Partial)
    }
}

/// Result of one base-gauge computation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseGauge {
    /// `K` from Eq. 10, or `None` when it could not be formed.
    pub k_base: Option<f64>,
    pub delta_h: Aggregate,
    pub delta_s: Aggregate,
    /// The window length actually used, in seconds.
    pub tau_secs: f64,
    pub confidence: Confidence,
    pub provenance: Provenance,
}

impl BaseGauge {
    /// The value to feed downstream formulas, with `None` collapsed to `0.0`.
    /// Callers that care about the difference must check `confidence` first —
    /// which is why this is a separate, explicitly-named method.
    pub fn value_or_zero(&self) -> f64 {
        self.k_base.unwrap_or(0.0)
    }
}

/// Compute the base K-gauge for one window.
pub fn compute_base(obs: &Observables, cfg: &GaugeConfig) -> BaseGauge {
    let delta_h = operational_aggregate(obs);
    let delta_s = network_aggregate(obs, cfg);

    let tau = if obs.window_secs > 0.0 { obs.window_secs } else { crate::constants::DEFAULT_TAU_SECS };

    let confidence = if obs.is_quiescent() {
        Confidence::NoEvidence
    } else {
        match (delta_h.total(), delta_s.total()) {
            (Some(_), Some(_)) => {
                if delta_h.is_complete() && delta_s.is_complete() {
                    Confidence::Full
                } else {
                    Confidence::Partial
                }
            }
            _ => Confidence::None,
        }
    };

    let k_base = match (delta_h.total(), delta_s.total()) {
        (Some(h), Some(s)) => {
            let product = h * s;
            if product.is_finite() && product >= 0.0 {
                let k = 2.0 * std::f64::consts::PI * product.sqrt() / tau;
                if k.is_finite() {
                    Some(k)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };

    let provenance = delta_h.provenance().worst(delta_s.provenance());

    BaseGauge { k_base, delta_h, delta_s, tau_secs: tau, confidence, provenance }
}

/// `dH` — operational stress local to this node.
fn operational_aggregate(obs: &Observables) -> Aggregate {
    let submitted = obs.submitted_delta();
    let accepted = obs.accepted_delta();
    let rejection = if submitted > 0 {
        let r = 1.0 - (accepted as f64 / submitted as f64);
        Channel::present("rejection_ratio", r, "share of submitted mining solutions this node rejected")
    } else {
        Channel::absent("rejection_ratio", "no mining solutions were submitted in this window")
    };

    let bin = obs.bytes_in_delta();
    let bout = obs.bytes_out_delta();
    let total = bin + bout;
    let asymmetry = if total > 0 {
        let a = (bin as f64 - bout as f64).abs() / total as f64;
        Channel::present("traffic_asymmetry", a, "imbalance between P2P bytes in and out")
    } else {
        Channel::absent("traffic_asymmetry", "no P2P traffic in this window")
    };

    let churn = if obs.previous.peer_count > 0 {
        let c = (obs.current.peer_count as f64 - obs.previous.peer_count as f64).abs()
            / obs.previous.peer_count as f64;
        Channel::present("peer_churn", c, "relative change in connected peer count")
    } else {
        Channel::absent("peer_churn", "no previous peer count to compare against")
    };

    Aggregate { name: Cow::Borrowed("delta_h"), channels: vec![rejection, asymmetry, churn] }
}

/// `ds` — stress in this node's relationship to the rest of the network.
fn network_aggregate(obs: &Observables, cfg: &GaugeConfig) -> Aggregate {
    // A network height of zero means "we have not heard a peer height", not
    // "we are at the tip". Reporting 0.0 divergence there is the single most
    // misleading thing this gauge could do, so it reports nothing instead.
    let divergence = if obs.current.network_height > 0 {
        let d = (obs.current.network_height as f64 - obs.current.local_height as f64).abs()
            / obs.current.network_height as f64;
        Channel::present("sync_divergence", d, "relative gap between local height and best known peer height")
    } else {
        Channel::absent(
            "sync_divergence",
            "no peer height has been observed — this is unknown, NOT in-sync",
        )
    };

    let rate = if cfg.target_block_rate_bps > 0.0 && obs.window_secs > 0.0 {
        let observed = obs.height_delta() as f64 / obs.window_secs;
        let d = (observed - cfg.target_block_rate_bps).abs() / cfg.target_block_rate_bps;
        Channel::present("block_rate_deviation", d, "relative deviation of observed block rate from target")
    } else {
        Channel::absent("block_rate_deviation", "no target block rate or no window duration configured")
    };

    Aggregate { name: Cow::Borrowed("delta_s"), channels: vec![divergence, rate] }
}

/// The base gauge as a tracked scalar, for composing with the enhancement.
pub fn tracked_base(g: &BaseGauge) -> Tracked<f64> {
    match g.k_base {
        Some(k) => Tracked { value: k, provenance: g.provenance, note: Cow::Borrowed("K_base, Eq. 10") },
        None => Tracked::unavailable(0.0, "K_base could not be formed from this window"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observables::CounterSample;

    fn busy_obs() -> Observables {
        Observables {
            previous: CounterSample {
                mining_submitted: 100,
                mining_accepted: 100,
                p2p_bytes_in: 1_000,
                p2p_bytes_out: 1_000,
                peer_count: 6,
                local_height: 1_000,
                network_height: 1_000,
            },
            current: CounterSample {
                mining_submitted: 200,
                mining_accepted: 200,
                p2p_bytes_in: 2_000,
                p2p_bytes_out: 2_000,
                peer_count: 6,
                local_height: 1_060,
                network_height: 1_060,
            },
            window_secs: 60.0,
            ..Observables::default()
        }
    }

    #[test]
    fn perfectly_healthy_node_reads_zero() {
        let obs = busy_obs();
        let g = compute_base(&obs, &GaugeConfig::quillon());
        // No rejections, symmetric traffic, no churn, in sync, exactly 1 bps.
        assert_eq!(g.confidence, Confidence::Full);
        assert!(g.k_base.unwrap() < 1e-12, "expected ~0, got {:?}", g.k_base);
    }

    #[test]
    fn idle_node_is_no_evidence_not_health() {
        let obs = Observables::default();
        let g = compute_base(&obs, &GaugeConfig::quillon());
        assert_eq!(g.confidence, Confidence::NoEvidence);
        // The number is still zero — but the confidence says why.
        assert!(!g.confidence.is_actionable());
    }

    #[test]
    fn unknown_peer_height_is_not_reported_as_synced() {
        let mut obs = busy_obs();
        obs.current.network_height = 0;
        let g = compute_base(&obs, &GaugeConfig::quillon());
        let ch = g
            .delta_s
            .channels
            .iter()
            .find(|c| c.name == "sync_divergence")
            .unwrap();
        assert!(!ch.is_present());
        assert_eq!(ch.provenance, Provenance::Unavailable);
        assert_eq!(g.confidence, Confidence::Partial);
        assert_eq!(g.delta_s.missing_channels(), vec!["sync_divergence".to_string()]);
    }

    #[test]
    fn geometric_mean_needs_both_sides() {
        // Heavy operational stress, zero network stress -> K stays 0.
        let mut obs = busy_obs();
        obs.current.mining_accepted = 100; // every solution rejected
        let g = compute_base(&obs, &GaugeConfig::quillon());
        assert!(g.delta_h.total().unwrap() > 0.9);
        assert!(g.delta_s.total().unwrap() < 1e-12);
        assert!(g.k_base.unwrap() < 1e-12);
    }

    #[test]
    fn reproduces_eq10_by_hand() {
        // dH = 1.0 (all rejected), ds = 1.0 (block rate double target)
        let obs = Observables {
            previous: CounterSample {
                mining_submitted: 0,
                mining_accepted: 0,
                peer_count: 0,
                p2p_bytes_in: 0,
                p2p_bytes_out: 0,
                local_height: 0,
                network_height: 0,
            },
            current: CounterSample {
                mining_submitted: 10,
                mining_accepted: 0,
                peer_count: 0,
                p2p_bytes_in: 0,
                p2p_bytes_out: 0,
                local_height: 120,
                network_height: 120,
            },
            window_secs: 60.0,
            ..Observables::default()
        };
        let g = compute_base(&obs, &GaugeConfig::quillon());
        // dH: rejection 1.0 (asymmetry + churn absent)
        assert!((g.delta_h.total().unwrap() - 1.0).abs() < 1e-12);
        // ds: divergence 0.0 + rate |2-1|/1 = 1.0
        assert!((g.delta_s.total().unwrap() - 1.0).abs() < 1e-12);
        // K = 2*pi*sqrt(1)/60
        let expected = 2.0 * std::f64::consts::PI / 60.0;
        assert!((g.k_base.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn wrong_target_rate_manufactures_stress() {
        // SIGIL's live steady-state rate is ~0.83 bps. Judged against the
        // often-quoted 6.28 bps catch-up figure it reports a rate deviation of
        // 0.87 out of nothing being wrong.
        let mut obs = busy_obs();
        obs.current.local_height = obs.previous.local_height + 50; // 0.83 bps
        let wrong = compute_base(&obs, &GaugeConfig::sigil().with_target_block_rate(6.28));
        let right = compute_base(&obs, &GaugeConfig::sigil());
        let wrong_dev = wrong
            .delta_s
            .channels
            .iter()
            .find(|c| c.name == "block_rate_deviation")
            .unwrap()
            .value
            .unwrap();
        let right_dev = right
            .delta_s
            .channels
            .iter()
            .find(|c| c.name == "block_rate_deviation")
            .unwrap()
            .value
            .unwrap();
        assert!(wrong_dev > 0.8, "got {wrong_dev}");
        assert!(right_dev < 0.05, "got {right_dev}");
    }

    #[test]
    fn aggregate_reports_missing_channels() {
        let obs = Observables::default();
        let g = compute_base(&obs, &GaugeConfig::quillon());
        assert_eq!(g.delta_h.present_channels(), 0);
        assert_eq!(g.delta_h.provenance(), Provenance::Unavailable);
        assert!(g.delta_h.total().is_none());
    }
}
