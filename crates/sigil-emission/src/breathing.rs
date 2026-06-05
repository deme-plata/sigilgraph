//! breathing.rs — the SIGIL emission **breathing observer + band enforcer**.
//!
//! Quillon ran a stateful PID controller that *actuated* issuance — `u(t) =
//! Kp·e(t) + Ki·∫e(t)dt + Kd·de/dt` feeding back into how much was minted. That
//! mutable "emission state" is precisely what drifted: it once started issuance
//! at zero and emitted the **wrong rewards for 3 days** before anyone noticed.
//!
//! SIGIL refuses to actuate. Emission is a pure function of height (see
//! [`crate::cumulative_emission`]) — there is no controller state to drift. We
//! keep only the *good* half of the breathing dashboard: the **error band** and
//! the **stability telemetry**, repurposed as a fail-loud **enforcer**. The
//! controller here is an observer that compares realized supply (read back from
//! the committed `wallet_state_root`) against the deterministic target and
//! **halts** the moment drift escapes the band.
//!
//! This is the postmortem fix by construction: a 3-day drift becomes a
//! block-1 halt. The target curve is the law; the band is the alarm.

use crate::cumulative_emission;

/// Tolerance band around the deterministic target, in basis points
/// (±2% = 200 bps) — the same ±2% error band drawn on the breathing dashboard.
pub const ERROR_BAND_BPS: i128 = 200;

/// A single breath: realized supply vs the deterministic target at `height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmissionTelemetry {
    pub height: u64,
    /// Deterministic ideal — the dashed "target supply" line.
    pub target: u128,
    /// Actual minted native supply, read from the committed state root.
    pub realized: u128,
    /// `e(t) = target − realized` (signed; positive = under-emitted).
    pub error: i128,
    /// `error` as signed basis points of `target`.
    pub error_bps: i128,
    /// Whether `|error_bps| ≤ ERROR_BAND_BPS`.
    pub within_band: bool,
}

/// A fail-loud emission fault — realized supply escaped the band. Carrying the
/// numbers so the halt message names the exact drift (no silent correction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmissionFault {
    pub height: u64,
    pub target: u128,
    pub realized: u128,
    pub error_bps: i128,
}

impl core::fmt::Display for EmissionFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "🚨 EMISSION BAND BREACH at height {}: target={} realized={} drift={}bps (band ±{}bps) — HALT",
            self.height, self.target, self.realized, self.error_bps, ERROR_BAND_BPS
        )
    }
}

/// Take one breath: measure realized vs the deterministic target at `height`.
/// Pure; no state. `realized` is the minted native supply from the committed root.
pub fn breathe(height: u64, realized: u128) -> EmissionTelemetry {
    let target = cumulative_emission(height);
    let error = target as i128 - realized as i128;
    let error_bps = if target == 0 {
        // No supply should exist yet; any realized supply is an infinite-bps breach.
        if realized == 0 { 0 } else { i128::MAX }
    } else {
        error.saturating_mul(10_000) / target as i128
    };
    let within_band = error_bps != i128::MAX && error_bps.abs() <= ERROR_BAND_BPS;
    EmissionTelemetry { height, target, realized, error, error_bps, within_band }
}

/// The chokepoint enforcement. Realized supply MUST track the deterministic
/// target within ±[`ERROR_BAND_BPS`]. Returns `Err` (fail loud) the instant it
/// doesn't — this is the guard Quillon lacked. Call it wherever the realized
/// native supply is committed (the `commit_state_transition` chokepoint).
pub fn enforce_emission_band(height: u64, realized: u128) -> Result<EmissionTelemetry, EmissionFault> {
    let t = breathe(height, realized);
    if t.within_band {
        Ok(t)
    } else {
        Err(EmissionFault { height: t.height, target: t.target, realized: t.realized, error_bps: t.error_bps })
    }
}

/// Stability metrics distilled from a run of breaths — the dashboard's
/// "system status" panel (overshoot, settling, steady-state error). Diagnostic
/// only; it changes nothing on chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BreathingReport {
    pub samples: usize,
    /// Worst over-emission seen, in bps (max negative error → positive overshoot).
    pub overshoot_bps: i128,
    /// Samples until the series entered the band and stayed (settling time).
    pub settling_samples: usize,
    /// `|error_bps|` of the final sample (steady-state error).
    pub steady_state_bps: i128,
    /// True iff every sample is within band.
    pub stable: bool,
}

/// Analyze an ordered series of breaths (oldest → newest).
pub fn analyze(series: &[EmissionTelemetry]) -> BreathingReport {
    if series.is_empty() {
        return BreathingReport::default();
    }
    let overshoot_bps = series
        .iter()
        .map(|t| (-t.error_bps).max(0)) // realized > target ⇒ overshoot
        .max()
        .unwrap_or(0);
    // settling: index after the last out-of-band sample.
    let last_oob = series.iter().rposition(|t| !t.within_band);
    let settling_samples = match last_oob {
        Some(i) => i + 1,
        None => 0,
    };
    let last = series.last().unwrap();
    BreathingReport {
        samples: series.len(),
        overshoot_bps,
        settling_samples,
        steady_state_bps: last.error_bps.abs(),
        stable: series.iter().all(|t| t.within_band),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HALVING_INTERVAL;

    /// The pure schedule tracks itself exactly: zero error, always in band.
    #[test]
    fn deterministic_tracks_exactly() {
        for h in [1_000u64, HALVING_INTERVAL, HALVING_INTERVAL * 3 + 7] {
            let realized = cumulative_emission(h);
            let t = breathe(h, realized);
            assert_eq!(t.error, 0);
            assert!(t.within_band);
            assert!(enforce_emission_band(h, realized).is_ok());
        }
    }

    /// The Quillon failure, replayed: issuance "started at zero" so realized
    /// stays 0 while the target has grown. The band enforcer HALTS — what would
    /// have been 3 days of wrong rewards becomes an immediate fault.
    #[test]
    fn quillon_failure_is_caught() {
        let h = HALVING_INTERVAL / 2; // target is large here
        let realized = 0; // wrong rewards / zero issuance
        let r = enforce_emission_band(h, realized);
        assert!(r.is_err(), "zero-issuance drift must fail loud");
        let fault = r.unwrap_err();
        assert!(fault.error_bps.abs() >= ERROR_BAND_BPS);
    }

    /// 1% under-emission is inside the ±2% band → allowed (jitter, not a bug).
    #[test]
    fn small_drift_within_band() {
        let h = HALVING_INTERVAL;
        let target = cumulative_emission(h);
        let realized = target - target / 100; // 1% under
        assert!(breathe(h, realized).within_band);
        assert!(enforce_emission_band(h, realized).is_ok());
    }

    /// 3% over-emission escapes the band → fault.
    #[test]
    fn drift_over_band_faults() {
        let h = HALVING_INTERVAL;
        let target = cumulative_emission(h);
        let realized = target + target * 3 / 100; // 3% over
        let t = breathe(h, realized);
        assert!(!t.within_band);
        assert!(t.error_bps < -ERROR_BAND_BPS, "over-emission is negative error");
        assert!(enforce_emission_band(h, realized).is_err());
    }

    /// At genesis the target is 0: zero realized is fine, any realized is a breach.
    #[test]
    fn genesis_band() {
        assert!(enforce_emission_band(0, 0).is_ok());
        assert!(enforce_emission_band(0, 1).is_err());
    }

    /// A breath series that drifts then recovers reports overshoot + settling.
    #[test]
    fn report_overshoot_and_settling() {
        let h = HALVING_INTERVAL;
        let target = cumulative_emission(h);
        let series = vec![
            breathe(h, target + target * 5 / 100), // 5% over (out of band)
            breathe(h, target + target / 100),     // 1% over (in band)
            breathe(h, target),                     // exact
        ];
        let rep = analyze(&series);
        assert!(rep.overshoot_bps >= 400, "should see the early overshoot");
        assert_eq!(rep.settling_samples, 1, "settles after the first sample");
        assert_eq!(rep.steady_state_bps, 0);
        assert!(!rep.stable, "one sample was out of band");
    }
}
