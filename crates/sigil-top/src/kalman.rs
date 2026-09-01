//! A tiny 1-D Kalman filter for smoothing the noisy sync-rate signal into a stable ETA.
//!
//! Extracted verbatim from main.rs (v0.33.3). Self-contained: no chain/UI state, pure f64
//! arithmetic, so it lives on its own and carries its own stability test.

/// v0.33.3: a tiny 1D Kalman filter that smooths the noisy 10s-window blk/s into a stable
/// rate estimate, used for a steady time-to-sync ETA (raw rate jitters too much to divide by).
/// Constant-value model: predict adds process noise q; update blends the measurement with
/// gain k = p/(p+r). Larger r = trust the model more = smoother. Tuned for block rates.
#[derive(Clone)]
pub(crate) struct Kalman1D { pub(crate) x: f64, p: f64, q: f64, r: f64, init: bool }
impl Kalman1D {
    pub(crate) fn new() -> Self { Self { x: 0.0, p: 1.0, q: 6.0, r: 180.0, init: false } }

    /// Force the estimate to `z`, discarding the accumulated model confidence.
    ///
    /// For a genuine REGIME CHANGE rather than a noisy sample. The filter's whole premise
    /// is that the underlying rate is roughly constant and the measurement is noisy; when
    /// sync leaves bulk import and starts following the frontier, that premise is simply
    /// false — the rate really did drop by two or three orders of magnitude, and slewing
    /// toward it is not smoothing, it is lying slowly.
    pub(crate) fn reset_to(&mut self, z: f64) {
        if !z.is_finite() { return; }
        self.x = z.max(0.0);
        self.p = 1.0;
        self.init = true;
    }

    pub(crate) fn update(&mut self, z: f64) -> f64 {
        if !z.is_finite() { return self.x; }
        if !self.init { self.x = z; self.init = true; return self.x; }
        self.p += self.q;                       // predict
        // ADAPTIVE GAIN. With the fixed q=6/r=180 pair the steady-state gain is
        // k = 36/216 = 0.167, so each sample moves the estimate only a sixth of the way to
        // the measurement — ~34 samples to cross a 400x drop. That is the right amount of
        // scepticism for NOISE and far too much for a real change. A residual much larger
        // than the estimate itself is not noise (the measurement noise model says a sample
        // lands near x); inflating p in proportion lets the filter believe the new
        // measurement within a couple of samples and then settle back to being sceptical.
        let resid = (z - self.x).abs();
        let scale = self.x.abs().max(z.abs()).max(1.0);
        if resid > 0.5 * scale {
            self.p += self.r * (resid / scale).min(4.0);
        }
        let k = self.p / (self.p + self.r);     // Kalman gain
        self.x += k * (z - self.x);             // correct
        self.p *= 1.0 - k;
        self.x
    }
}

#[cfg(test)]
mod kalman_tests {
    use super::Kalman1D;

    #[test]
    fn filter_is_stable_and_never_produces_garbage() {
        // First finite sample initializes the estimate exactly.
        let mut f = Kalman1D::new();
        assert_eq!(f.update(100.0), 100.0);

        // Non-finite samples are rejected: return the current estimate, no corruption
        // (a NaN/inf ETA on screen — or a poisoned filter state — is the failure this guards).
        assert_eq!(f.update(f64::NAN), 100.0);
        assert_eq!(f.update(f64::INFINITY), 100.0);
        assert!(f.update(101.0).is_finite());

        // A small blip is heavily DAMPENED (steady-state gain ~0.04) — barely moves.
        let mut g = Kalman1D::new();
        g.update(100.0);
        let after = g.update(110.0);
        assert!((100.0..101.0).contains(&after), "small blip must be smoothed, got {after}");

        // A big regime change (a ~400x drop) is FOLLOWED fast via the adaptive gain.
        let mut h = Kalman1D::new();
        h.update(400.0);
        let dropped = h.update(1.0);
        assert!(dropped < 300.0, "a real drop must be believed quickly, got {dropped}");

        // reset_to forces the estimate; clamps negatives to 0; ignores non-finite input.
        let mut r = Kalman1D::new();
        r.update(400.0);
        r.reset_to(1.0);
        let v = r.update(1.0);
        assert!(v.is_finite() && (0.9..1.1).contains(&v), "reset_to should force ~1, got {v}");
        let mut n = Kalman1D::new();
        n.update(5.0);
        n.reset_to(-9.0);
        assert!(n.update(0.0) >= 0.0, "negative reset clamps to 0");
        n.reset_to(f64::NAN); // ignored — no corruption
        assert!(n.update(0.0).is_finite());

        // Output stays finite across a long, wildly-noisy sequence.
        let mut s = Kalman1D::new();
        for z in [10.0, 1000.0, 0.001, 500.0, 2.0, 800.0, 0.5] {
            assert!(s.update(z).is_finite());
        }
    }
}
