//! Commitment depth and irreversibility — Eq. 19, 20, 23, 24.
//!
//! ```text
//!     d_commit(v)   = |{w in V : w is a descendant of v}|              (19)
//!     Lambda_commit = 1 - exp(-d_commit(tip) / (kappa * tau_confirm))  (20)
//!     f_irrev       = |{v in W : d_commit(v) > D_reorg}| / |W|         (23)
//!     D_reorg       = kappa * ceil(log2(1/epsilon))                    (24)
//! ```
//!
//! The idea is Lloyd's: a system becomes classical as it accumulates
//! irreversible operations. A block becomes final as it accumulates
//! descendants. Both are one-way ratchets, and `Lambda` is just that ratchet
//! squeezed into `[0, 1]`.
//!
//! ## Two traps, both of which produce a confident wrong number
//!
//! **Trap 1 — `d_commit(tip)` is zero by construction.** Eq. 20 asks for the
//! descendant count *of the chain tip*, and nothing is ever built on the tip:
//! that is what makes it the tip. Read literally, `Lambda = 0` always, and
//! Eq. 25 then divides by the clamp and reports `K_enhanced = 100 * K_base`
//! forever. What Figure 6 actually plots is `Lambda` for a block *near* the
//! tip, so the reference block has to be named explicitly. That is
//! [`CommitmentBasis`].
//!
//! **Trap 2 — a short window pins `f_irrev` to zero.** Eq. 23 counts blocks in
//! a window `W` deeper than `D_reorg`. The deepest block in a window of `L`
//! blocks has depth at most `L - 1`. So whenever `L <= D_reorg` the answer is
//! zero *regardless of how settled the chain is*. On a 60-second window at
//! 3.5 blocks/s, `L ~ 210 < 360`, and `f_irrev` is structurally pinned to 0.
//! Publishing that as "the chain is unsettled" is a measurement artefact, not
//! a finding. [`Commitment::f_irrev`] returns `None` in that case and says so.

use serde::{Deserialize, Serialize};

use crate::config::GaugeConfig;
use crate::observables::Observables;
use crate::provenance::{Note, Provenance, Tracked};
use std::borrow::Cow;

/// Which block `Lambda_commit` is measured for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "basis", content = "arg")]
pub enum CommitmentBasis {
    /// Descendants of the block that was the tip when the window opened. This
    /// is what the reference implementation computes. It is well defined, but
    /// note that it makes `Lambda` a function of *window length times block
    /// rate*, not of chain health: it saturates at a fixed value and stays
    /// there. See [`Commitment::lambda_ceiling`].
    WindowBase,
    /// Descendants of the block `d` blocks below the tip. Use this when you
    /// want "how settled is a transaction I saw `d` blocks ago".
    ReferenceDepth(u64),
    /// The literal reading of Eq. 20: `d_commit(tip) = 0`. Always yields
    /// `Lambda = 0`. Provided so the degenerate case is explicit rather than
    /// accidental.
    ChainTip,
}

impl Default for CommitmentBasis {
    fn default() -> Self {
        CommitmentBasis::WindowBase
    }
}

/// Everything the commitment layer produced this round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Commitment {
    /// `d_commit` for the chosen basis (Eq. 19).
    pub d_commit: u64,
    pub basis: CommitmentBasis,
    /// `Lambda_commit` in `[0, 1]` (Eq. 20).
    pub lambda: f64,
    /// `Lambda` after the `[lambda_min, 1]` clamp used by Eq. 25.
    pub lambda_clamped: f64,
    /// `1 / lambda_clamped` — the multiplier Eq. 25 applies to `K_base`.
    pub commitment_multiplier: f64,
    /// The largest `Lambda` this measurement setup could ever report, given how
    /// many blocks the window contains. If this is far below 1, `Lambda` is
    /// telling you about your window, not about your chain.
    pub lambda_ceiling: f64,
    /// `f_irrev` (Eq. 23), or `None` when the window is too short to answer.
    pub f_irrev: Option<f64>,
    /// Why `f_irrev` is `None`, when it is.
    pub f_irrev_note: Note,
    /// `D_reorg` (Eq. 24).
    pub reorg_depth_bound: u64,
    /// Blocks in the window.
    pub window_blocks: usize,
    pub provenance: Provenance,
}

impl Commitment {
    pub fn tracked_multiplier(&self) -> Tracked<f64> {
        Tracked {
            value: self.commitment_multiplier,
            provenance: self.provenance,
            note: Cow::Borrowed("1/Lambda_commit multiplier, Eq. 25"),
        }
    }

    /// True when `Lambda` is dominated by the measurement window rather than by
    /// the chain. Threshold is deliberately generous: anything under 0.9 means
    /// the window caps the answer meaningfully.
    pub fn lambda_is_window_limited(&self) -> bool {
        self.lambda_ceiling < 0.9
    }
}

/// Eq. 20, bare.
pub fn lambda_commit(d_commit: f64, scale: f64) -> f64 {
    if !d_commit.is_finite() || d_commit <= 0.0 || !scale.is_finite() || scale <= 0.0 {
        return 0.0;
    }
    (1.0 - (-d_commit / scale).exp()).clamp(0.0, 1.0)
}

/// Compute the commitment layer for one window.
pub fn compute(obs: &Observables, cfg: &GaugeConfig, basis: CommitmentBasis) -> Commitment {
    let scale = cfg.commitment_scale();
    let d_reorg = cfg.reorg_depth_bound();
    let height_delta = obs.height_delta();

    let d_commit = match basis {
        CommitmentBasis::WindowBase => height_delta,
        CommitmentBasis::ReferenceDepth(d) => d,
        CommitmentBasis::ChainTip => 0,
    };

    let lambda = lambda_commit(d_commit as f64, scale);
    let lambda_min = cfg.lambda_commit_min.clamp(1e-9, 1.0);
    let lambda_clamped = lambda.max(lambda_min);
    let commitment_multiplier = 1.0 / lambda_clamped;

    // The ceiling: with this basis, what is the largest d_commit the setup can
    // ever produce, and therefore the largest Lambda?
    let ceiling_d = match basis {
        CommitmentBasis::WindowBase => height_delta,
        CommitmentBasis::ReferenceDepth(d) => d,
        CommitmentBasis::ChainTip => 0,
    };
    let lambda_ceiling = lambda_commit(ceiling_d as f64, scale);

    // f_irrev, with the window-adequacy guard.
    let window_blocks = obs.window.len();
    let (f_irrev, f_irrev_note) = if window_blocks == 0 {
        (None, Cow::Borrowed("no block window supplied"))
    } else if !obs.window.spans_reorg_depth(d_reorg) {
        (
            None,
            Cow::Borrowed(
                "window is shorter than D_reorg — f_irrev would be 0 by construction, not by measurement",
            ),
        )
    } else {
        let tip = obs.window.tip_height().unwrap_or(0);
        let deep = obs
            .window
            .blocks
            .iter()
            .filter(|b| tip.saturating_sub(b.height) > d_reorg)
            .count();
        (
            Some(deep as f64 / window_blocks as f64),
            Cow::Borrowed("fraction of window blocks deeper than D_reorg"),
        )
    };

    // d_commit is a graph fact derived from measured heights; the constants
    // kappa and tau_confirm behind `scale` are protocol values, and
    // tau_confirm = 100 is only *proposed* in v4. So the best this can be is
    // Protocol-grade, never Measured.
    let provenance = Provenance::Derived.worst(Provenance::Protocol);

    Commitment {
        d_commit,
        basis,
        lambda,
        lambda_clamped,
        commitment_multiplier,
        lambda_ceiling,
        f_irrev,
        f_irrev_note,
        reorg_depth_bound: d_reorg,
        window_blocks,
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observables::{BlockFact, BlockWindow, CounterSample};

    fn obs_with(height_delta: u64, window_len: u64) -> Observables {
        let blocks: Vec<BlockFact> = (0..window_len)
            .map(|i| BlockFact {
                height: 1_000_000 + i,
                producer: [1u8; 32],
                merge_parent_count: 0,
                blue_score: Some(i),
                is_blue: Some(true),
            })
            .collect();
        Observables {
            previous: CounterSample { local_height: 1_000_000, ..Default::default() },
            current: CounterSample {
                local_height: 1_000_000 + height_delta,
                ..Default::default()
            },
            window_secs: 60.0,
            window: BlockWindow::new(blocks),
            ..Observables::default()
        }
    }

    #[test]
    fn lambda_matches_paper_figure6_points() {
        let scale = 18.0 * 100.0; // 1800
        // Fresh tip d = 5 -> Lambda ~ 0.003
        let l5 = lambda_commit(5.0, scale);
        assert!((l5 - 0.00277).abs() < 1e-4, "got {l5}");
        // d = kappa * tau_confirm -> Lambda = 1 - 1/e = 0.632
        let lc = lambda_commit(scale, scale);
        assert!((lc - (1.0 - (-1.0f64).exp())).abs() < 1e-12, "got {lc}");
    }

    #[test]
    fn chain_tip_basis_is_degenerate_and_says_so() {
        let obs = obs_with(200, 200);
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::ChainTip);
        assert_eq!(c.d_commit, 0);
        assert_eq!(c.lambda, 0.0);
        // Clamped, so Eq. 25 multiplies by exactly the cap.
        assert!((c.commitment_multiplier - 100.0).abs() < 1e-9);
        assert!(c.lambda_is_window_limited());
    }

    #[test]
    fn window_base_lambda_is_pinned_by_window_length() {
        // A 60s window on a ~3.46 bps chain sees ~208 blocks. Lambda then
        // saturates near 0.11 and stays there no matter how healthy the chain
        // is -- so K_enhanced sits at ~9x K_base permanently.
        let obs = obs_with(208, 208);
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        assert!((c.lambda - 0.109).abs() < 0.01, "lambda was {}", c.lambda);
        assert!(
            c.commitment_multiplier > 8.0 && c.commitment_multiplier < 10.0,
            "multiplier was {}",
            c.commitment_multiplier
        );
        assert!(c.lambda_is_window_limited());
    }

    #[test]
    fn short_window_cannot_answer_f_irrev() {
        let obs = obs_with(208, 208); // 208 < D_reorg = 360
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        assert!(c.f_irrev.is_none());
        assert!(c.f_irrev_note.contains("shorter than D_reorg"));
    }

    #[test]
    fn long_window_answers_f_irrev() {
        let obs = obs_with(1000, 1000);
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        let f = c.f_irrev.expect("window of 1000 > D_reorg 360 must answer");
        // Blocks deeper than 360 below the tip: heights 0..=638 of 1000 => 0.639
        assert!((f - 0.639).abs() < 0.01, "f_irrev was {f}");
    }

    #[test]
    fn fully_settled_chain_approaches_f_irrev_one() {
        let obs = obs_with(100_000, 100_000);
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        assert!(c.f_irrev.unwrap() > 0.99);
    }

    #[test]
    fn reference_depth_basis_gives_a_real_lambda() {
        let obs = obs_with(208, 400);
        let cfg = GaugeConfig::default();
        // "How settled is a block 5000 deep?" -> deeply.
        let c = compute(&obs, &cfg, CommitmentBasis::ReferenceDepth(5_000));
        assert!(c.lambda > 0.93, "lambda {}", c.lambda);
        assert!(c.commitment_multiplier < 1.1);
        assert!(!c.lambda_is_window_limited());
    }

    #[test]
    fn empty_window_reports_unavailable_not_zero() {
        let mut obs = obs_with(0, 0);
        obs.window = BlockWindow::default();
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        assert!(c.f_irrev.is_none());
        assert_eq!(c.f_irrev_note, "no block window supplied");
    }

    #[test]
    fn lambda_rejects_nonsense() {
        assert_eq!(lambda_commit(-5.0, 1800.0), 0.0);
        assert_eq!(lambda_commit(5.0, 0.0), 0.0);
        assert_eq!(lambda_commit(f64::NAN, 1800.0), 0.0);
    }

    #[test]
    fn commitment_is_never_measured_grade() {
        let obs = obs_with(208, 400);
        let c = compute(&obs, &GaugeConfig::default(), CommitmentBasis::WindowBase);
        assert_eq!(c.provenance, Provenance::Protocol);
    }
}
