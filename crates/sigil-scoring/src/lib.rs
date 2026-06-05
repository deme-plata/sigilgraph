// sigil-scoring — three-axis composite scoring for SIGIL.
//
// Combines:
//   SAP     — 5 dims (contribution/latency/stake/accuracy/uptime), ported from flux-p2p::sap
//   X-Algo  — 5 dims (temporal/consensus/quality/topology/economic), ported from flux-p2p::x_algo
//   K-param — physics engine (correlation+drift+oscillation+phase-coherence+noise), ported from
//             q-narwhalknight void-walker::k_parameter with SIGIL semantics
//
// Composite default: 0.5·SAP + 0.3·X-Algo + 0.2·K-correlation.
//
// Generic over subject key K so the same machinery scores:
//   - Validators       (K = ValidatorId)
//   - Pools            (K = PoolId)        — used in DEX quote/swap scoring
//   - Wallets          (K = WalletId)      — used in counterparty trust
//   - LP positions     (K = LpPositionId)  — used in fee-share quality
//
// DEX-specific scorers live in `dex` and compose all three axes.

pub mod subject;
pub mod kparam;
pub mod dex;

pub use subject::{
    SubjectScoreTable, SubjectScore, SapComponents, SapWeights,
    XAlgoTable, XAlgoScore, XAlgoDimensions, XAlgoWeights,
};
pub use kparam::{KParameterEngine, KParameterState, KStabilityReport};
pub use dex::{PoolHealthScore, SwapQualityScore, LpReputationScore, score_swap, SwapInput};

/// Tag identifying what kind of subject is being scored. Carried alongside
/// SubjectScore for downstream rendering (UI badges, RPC payloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SigilSubject {
    Validator,
    Pool,
    Wallet,
    LpPosition,
}

/// Tunable weights for the 3-axis composite. Defaults: 0.5/0.3/0.2.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompositeWeights {
    pub sap: f64,
    pub xalgo: f64,
    pub kparam: f64,
}

impl Default for CompositeWeights {
    fn default() -> Self {
        CompositeWeights { sap: 0.5, xalgo: 0.3, kparam: 0.2 }
    }
}

/// Default 3-axis composite: 0.5·SAP + 0.3·X-Algo + 0.2·K. Clamped to [0,1].
pub fn composite_score(sap: f64, xalgo: f64, kparam: f64) -> f64 {
    composite_score_weighted(sap, xalgo, kparam, CompositeWeights::default())
}

/// 3-axis composite with explicit weights. Clamped to [0,1]. Weights are
/// not required to sum to 1.0 — caller is responsible for normalization
/// if interpretive meaning matters.
pub fn composite_score_weighted(
    sap: f64,
    xalgo: f64,
    kparam: f64,
    w: CompositeWeights,
) -> f64 {
    (sap * w.sap + xalgo * w.xalgo + kparam * w.kparam).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_default_weights_sum_to_one() {
        let w = CompositeWeights::default();
        assert!((w.sap + w.xalgo + w.kparam - 1.0).abs() < 1e-9);
    }

    #[test]
    fn composite_score_clamps_high() {
        // All-ones with default weights = 1.0 exactly.
        let s = composite_score(1.0, 1.0, 1.0);
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn composite_score_clamps_low() {
        let s = composite_score(0.0, 0.0, 0.0);
        assert!(s == 0.0);
    }

    #[test]
    fn composite_score_basic_mix() {
        // sap=0.8, xalgo=0.6, k=0.4 → 0.4 + 0.18 + 0.08 = 0.66
        let s = composite_score(0.8, 0.6, 0.4);
        assert!((s - 0.66).abs() < 1e-9, "got {}", s);
    }
}
