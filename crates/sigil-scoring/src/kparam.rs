// K-Parameter physics engine, SIGIL-flavoured port.
//
// Ported from q-narwhalknight::void-walker::k_parameter (Quillon Graph repo).
// The original was attosecond-precision EEG-driven; here we re-cast the
// inputs for chain semantics:
//
//   void-walker            →  SIGIL
//   ──────────────────────────────────────────────────────────────────────────
//   EEG amplitude (mV)     →  network-activity amplitude (txs/s, normalised ~50)
//   intent (free string)   →  SigilEvent kind tag ("Send", "SwapExecuted", …)
//   attosecond clock       →  millisecond clock (chain temporal granularity)
//   ATTOSECOND constant    →  MILLI_TICK   (time-scaling)
//   PLANCK constant        →  NOISE_AMPLITUDE (quantum-noise stddev scale)
//
// The underlying math (correlation + drift_rate + oscillation + phase_coherence
// + Gaussian noise + sliding history) is preserved verbatim — only the input
// semantics and constants change.
//
// Output is a single `correlation` value on (roughly) [0, 1] after clamp,
// suitable as the third axis of the SIGIL composite score
// (see lib.rs::composite_score).

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::time::{SystemTime, UNIX_EPOCH};

use rand_distr::{Distribution, Normal};
use serde::{Deserialize, Serialize};

// Time-scaling constant. Original used ATTOSECOND = 1e-18 to scale attoseconds
// into the trig argument; for SIGIL we treat wall-clock milliseconds as our
// granularity. MILLI_TICK = 1e-3 keeps the oscillation argument numerically
// comparable when oscillation_freq is set in Hz (cycles/sec).
pub const MILLI_TICK: f64 = 1e-3;

// Noise amplitude scaling constant. Original used PLANCK = 6.626e-34 then
// scaled by 1e6. For SIGIL we expose a tunable constant ~1e-9 (very small
// but nonzero) — overrideable via KParameterEngine::with_noise_amplitude.
pub const DEFAULT_NOISE_AMPLITUDE: f64 = 1e-9;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Instantaneous K-parameter state. Stored per-event in the engine's history.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KParameterState {
    pub correlation: f64,
    pub drift_rate: f64,
    pub oscillation_freq: f64,
    pub quantum_noise: f64,
    pub phase_coherence: f64,
    pub timestamp_ms: u64,
}

impl KParameterState {
    pub fn new(base_correlation: f64) -> Self {
        KParameterState {
            correlation: base_correlation,
            drift_rate: 0.0,
            oscillation_freq: 1.0, // 1 Hz default — block-time order of magnitude
            quantum_noise: 0.0,
            phase_coherence: 0.95,
            timestamp_ms: 0,
        }
    }
}

/// K-Parameter physics engine. Drives a correlation series that responds to
/// network-activity amplitude + event-kind input, plus an internal oscillator
/// and Gaussian noise term. Stable over a long history of well-behaved input.
#[derive(Clone, Debug)]
pub struct KParameterEngine {
    pub current: KParameterState,
    pub baseline_k: f64,
    history: VecDeque<KParameterState>,
    max_history: usize,
    noise_amplitude: f64,
}

impl KParameterEngine {
    /// Create an engine with the given baseline correlation. A common
    /// neutral starting point is `0.5`.
    pub fn new(baseline_k: f64) -> Self {
        KParameterEngine {
            current: KParameterState::new(baseline_k),
            baseline_k,
            history: VecDeque::new(),
            max_history: 1000,
            noise_amplitude: DEFAULT_NOISE_AMPLITUDE,
        }
    }

    /// Override the noise amplitude (useful for tests or chains that want
    /// less / more jitter).
    pub fn with_noise_amplitude(mut self, amp: f64) -> Self {
        self.noise_amplitude = amp.max(0.0);
        self
    }

    /// Maximum number of states retained in history. Older states are
    /// dropped FIFO. Default 1000.
    pub fn with_max_history(mut self, n: usize) -> Self {
        self.max_history = n.max(10);
        self
    }

    /// Update the K-parameter from a chain event.
    ///
    /// * `network_activity_amplitude` — tx/s (or any normalised activity
    ///   signal); the original "EEG amplitude" channel. Range typically
    ///   0..~50.
    /// * `event_kind` — the SigilEvent variant name, e.g. "SwapExecuted".
    ///   Hashed with BLAKE3 to derive a small per-kind perturbation. Same
    ///   kind always perturbs in the same direction.
    pub fn update_event(
        &mut self,
        network_activity_amplitude: f64,
        event_kind: &str,
    ) -> KParameterState {
        let now = now_ms();

        let activity_influence = (network_activity_amplitude / 50.0).tanh() * 0.001;
        let kind_hash = blake3::hash(event_kind.as_bytes());
        let kind_influence = (kind_hash.as_bytes()[0] as f64 / 255.0 - 0.5) * 0.0005;

        let dt_ms = (now - self.current.timestamp_ms) as f64;
        let dt_scaled = dt_ms * MILLI_TICK;
        let oscillation = (self.current.oscillation_freq * dt_scaled * TAU).sin() * 0.0001;

        let noise = self.gen_noise();

        let new_correlation = self.baseline_k
            + activity_influence
            + kind_influence
            + oscillation
            + noise;

        let drift_rate = if self.history.is_empty() || dt_scaled == 0.0 {
            0.0
        } else {
            (new_correlation - self.current.correlation) / dt_scaled.max(1e-12)
        };

        self.current = KParameterState {
            correlation: new_correlation,
            drift_rate,
            oscillation_freq: self.current.oscillation_freq * (1.0 + activity_influence * 0.01),
            quantum_noise: noise,
            phase_coherence: (self.current.phase_coherence + activity_influence * 0.1)
                .clamp(0.0, 1.0),
            timestamp_ms: now,
        };

        self.history.push_back(self.current.clone());
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
        self.current.clone()
    }

    fn gen_noise(&self) -> f64 {
        // Gaussian with mean 0, stddev = sqrt(noise_amplitude) * 1e6.
        // The 1e6 scaling tracks the original (PLANCK-rooted) calibration.
        if self.noise_amplitude <= 0.0 { return 0.0; }
        let stddev = self.noise_amplitude.sqrt() * 1e6;
        match Normal::new(0.0, stddev) {
            Ok(n) => n.sample(&mut rand::thread_rng()),
            Err(_) => 0.0,
        }
    }

    /// Current correlation value. This is the scalar fed into the 3-axis
    /// composite as the K-axis input. Callers should clamp / normalise to
    /// [0,1] before feeding to composite_score.
    pub fn current_correlation(&self) -> f64 { self.current.correlation }

    /// Normalised correlation on [0, 1]. The raw correlation centres on
    /// baseline_k (typically 0.5) — this returns `correlation.clamp(0,1)`.
    pub fn normalised_correlation(&self) -> f64 {
        self.current.correlation.clamp(0.0, 1.0)
    }

    /// Phase coherence on [0,1] — the validator-alignment proxy.
    pub fn phase_coherence(&self) -> f64 { self.current.phase_coherence }

    /// History snapshot — useful for dashboards / time-series plots.
    pub fn history(&self) -> &VecDeque<KParameterState> { &self.history }

    /// Stability analysis over the retained history. Requires ≥10 samples.
    pub fn stability(&self) -> KStabilityReport {
        if self.history.len() < 10 {
            return KStabilityReport::insufficient_data();
        }
        let corrs: Vec<f64> = self.history.iter().map(|s| s.correlation).collect();
        let mean = corrs.iter().sum::<f64>() / corrs.len() as f64;
        let variance = corrs.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / corrs.len() as f64;
        let std_dev = variance.sqrt();

        // Trend = (later-half mean) - (earlier-half mean). Positive = rising
        // K-correlation across the window.
        let mid = corrs.len() / 2;
        let first_mean: f64 = corrs.iter().take(mid).sum::<f64>() / mid.max(1) as f64;
        let second_mean: f64 = corrs.iter().skip(mid).sum::<f64>() / (corrs.len() - mid).max(1) as f64;
        let trend = second_mean - first_mean;

        KStabilityReport {
            mean_correlation: mean,
            std_deviation: std_dev,
            trend,
            stability_score: (1.0 / (1.0 + std_dev * 100.0)).min(1.0),
            sample_count: self.history.len(),
        }
    }
}

/// Aggregated stability over the recent K-parameter history.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KStabilityReport {
    pub mean_correlation: f64,
    pub std_deviation: f64,
    pub trend: f64,
    pub stability_score: f64,
    pub sample_count: usize,
}

impl KStabilityReport {
    fn insufficient_data() -> Self {
        KStabilityReport { sample_count: 0, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_baseline_correlation() {
        let e = KParameterEngine::new(0.5);
        assert!((e.current_correlation() - 0.5).abs() < 1e-9);
        assert_eq!(e.history.len(), 0);
    }

    #[test]
    fn engine_update_grows_history() {
        let mut e = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        for i in 0..50 {
            e.update_event(10.0, "Send");
            assert_eq!(e.history.len(), i + 1);
        }
    }

    #[test]
    fn engine_history_caps_at_max() {
        let mut e = KParameterEngine::new(0.5)
            .with_noise_amplitude(0.0)
            .with_max_history(20);
        for _ in 0..100 {
            e.update_event(5.0, "Send");
        }
        assert_eq!(e.history.len(), 20);
    }

    #[test]
    fn engine_same_event_kind_consistent_perturbation() {
        // Two engines with noise=0 and identical event sequences should land
        // close (millisecond timing causes tiny dt-scaled oscillation diff).
        let mut e1 = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        let mut e2 = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        for _ in 0..10 {
            e1.update_event(10.0, "Send");
            e2.update_event(10.0, "Send");
        }
        // Allow some tolerance because timestamp_ms differs per call.
        let diff = (e1.current_correlation() - e2.current_correlation()).abs();
        assert!(diff < 0.01, "consistent kind drift too large: {diff}");
    }

    #[test]
    fn engine_normalised_correlation_clamped() {
        let mut e = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        e.update_event(0.0, "evt");
        let n = e.normalised_correlation();
        assert!((0.0..=1.0).contains(&n));
    }

    #[test]
    fn stability_insufficient_data() {
        let e = KParameterEngine::new(0.5);
        let r = e.stability();
        assert_eq!(r.sample_count, 0);
    }

    #[test]
    fn stability_reports_with_enough_samples() {
        let mut e = KParameterEngine::new(0.5).with_noise_amplitude(0.0);
        for _ in 0..20 {
            e.update_event(10.0, "Send");
        }
        let r = e.stability();
        assert_eq!(r.sample_count, 20);
        // With noise=0 + same input, std_dev should be tiny.
        assert!(r.std_deviation < 0.01);
        // Stability score should be high (near 1.0).
        assert!(r.stability_score > 0.5);
    }
}
