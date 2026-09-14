//! The gauge maths, ported line-for-line from the python prototype so the numbers do not move:
//! trailing-year z-scores (K_raw), a de-wobble model — linear trend + annual + semiannual (+ Chandler
//! 433 d for the pole) fitted by least squares over six years — and the residual z-score (K_resid),
//! plus the empirical ladder quantiles.

use crate::eop::{Key, Row};
use serde::Serialize;

pub const OMEGA0: f64 = 7.292115e-5; // rad/s nominal mean sidereal rate (IERS Conventions)
pub const I_EARTH: f64 = 8.034e37; // kg m², polar moment of inertia (Williams 1994; ASSUMED constant)
pub const ARCSEC: f64 = std::f64::consts::PI / 648000.0;
pub const R_EARTH: f64 = 6.371e6;
pub const DAYS_YEAR: f64 = 365.25;
pub const CHANDLER_DAYS: f64 = 433.0;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    pub mean: f64,
    pub std: f64,
}

pub fn mean_std(v: &[f64]) -> Stats {
    let n = v.len().max(1) as f64;
    let mean = v.iter().sum::<f64>() / n;
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len().saturating_sub(1).max(1) as f64);
    Stats { mean, std: var.sqrt() }
}

/// Observed rows with `key` present in (mjd_now − days, mjd_now].
pub fn window<'a>(rows: &'a [Row], mjd_now: f64, days: f64, key: Key) -> Vec<&'a Row> {
    rows.iter()
        .filter(|r| r.observed() && r.get(key).is_some() && mjd_now - days < r.mjd && r.mjd <= mjd_now)
        .collect()
}

/// Normal equations solved by Gaussian elimination with partial pivoting (8×8 at most).
pub fn lstsq(x: &[Vec<f64>], y: &[f64]) -> Vec<f64> {
    let n = x.first().map(|r| r.len()).unwrap_or(0);
    let mut m = vec![vec![0.0; n + 1]; n];
    for i in 0..n {
        for j in 0..n {
            m[i][j] = x.iter().map(|r| r[i] * r[j]).sum();
        }
        m[i][n] = x.iter().zip(y).map(|(r, yy)| r[i] * yy).sum();
    }
    for c in 0..n {
        let p = (c..n).max_by(|&a, &b| m[a][c].abs().partial_cmp(&m[b][c].abs()).unwrap()).unwrap();
        m.swap(c, p);
        if m[c][c] == 0.0 {
            continue;
        }
        for r in 0..n {
            if r != c {
                let k = m[r][c] / m[c][c];
                for j in 0..=n {
                    m[r][j] -= k * m[c][j];
                }
            }
        }
    }
    (0..n).map(|i| if m[i][i] != 0.0 { m[i][n] / m[i][i] } else { 0.0 }).collect()
}

pub fn basis(t: f64, chandler: bool) -> Vec<f64> {
    let w1 = 2.0 * std::f64::consts::PI / DAYS_YEAR;
    let w2 = 2.0 * w1;
    let wc = 2.0 * std::f64::consts::PI / CHANDLER_DAYS;
    let mut b = vec![1.0, t / DAYS_YEAR, (w1 * t).cos(), (w1 * t).sin(), (w2 * t).cos(), (w2 * t).sin()];
    if chandler {
        b.push((wc * t).cos());
        b.push((wc * t).sin());
    }
    b
}

#[derive(Debug, Clone, Serialize)]
pub struct Fit {
    pub coef: Vec<f64>,
    pub resid_std: f64,
    pub n: usize,
    pub model: String,
    pub chandler: bool,
}

pub fn fit(rows: &[Row], mjd_now: f64, key: Key, days: f64, chandler: bool) -> Fit {
    let w = window(rows, mjd_now, days, key);
    let x: Vec<Vec<f64>> = w.iter().map(|r| basis(r.mjd - mjd_now, chandler)).collect();
    let y: Vec<f64> = w.iter().map(|r| r.get(key).unwrap()).collect();
    let coef = lstsq(&x, &y);
    let resid: Vec<f64> = x
        .iter()
        .zip(&y)
        .map(|(xb, yy)| yy - coef.iter().zip(xb).map(|(c, b)| c * b).sum::<f64>())
        .collect();
    Fit {
        coef,
        resid_std: mean_std(&resid).std,
        n: w.len(),
        model: format!("trend + annual + semiannual{}", if chandler { " + Chandler 433 d" } else { "" }),
        chandler,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Models {
    pub xp: Fit,
    pub yp: Fit,
    pub lod: Fit,
    pub mjd_now: f64,
}

impl Models {
    pub fn build(rows: &[Row], mjd_now: f64) -> Models {
        let six = 6.0 * DAYS_YEAR;
        Models {
            xp: fit(rows, mjd_now, Key::Xp, six, true),
            yp: fit(rows, mjd_now, Key::Yp, six, true),
            lod: fit(rows, mjd_now, Key::Lod, six, false),
            mjd_now,
        }
    }
    pub fn fit(&self, k: Key) -> &Fit {
        match k {
            Key::Xp => &self.xp,
            Key::Yp => &self.yp,
            Key::Lod => &self.lod,
        }
    }
    pub fn value(&self, k: Key, mjd: f64) -> f64 {
        let f = self.fit(k);
        let xb = basis(mjd - self.mjd_now, f.chandler);
        f.coef.iter().zip(&xb).map(|(c, b)| c * b).sum()
    }
    /// The de-wobbled gauge at an arbitrary (xp, yp, lod) triple — used for forecasts too.
    pub fn k_resid_at(&self, mjd: f64, xp: f64, yp: f64, lod: f64) -> f64 {
        let z = |k: Key, v: f64| (v - self.value(k, mjd)) / self.fit(k).resid_std;
        (z(Key::Xp, xp).powi(2) + z(Key::Yp, yp).powi(2) + z(Key::Lod, lod).powi(2)).sqrt()
    }
    pub fn k_resid(&self, r: &Row) -> Option<f64> {
        r.lod.map(|lod| self.k_resid_at(r.mjd, r.xp, r.yp, lod))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub xp: Stats,
    pub yp: Stats,
    pub lod: Stats,
}

impl Reference {
    pub fn build(rows: &[Row], mjd_now: f64) -> Reference {
        let s = |k: Key| mean_std(&window(rows, mjd_now, DAYS_YEAR, k).iter().map(|r| r.get(k).unwrap()).collect::<Vec<_>>());
        Reference { xp: s(Key::Xp), yp: s(Key::Yp), lod: s(Key::Lod) }
    }
    pub fn k_raw(&self, r: &Row) -> Option<f64> {
        let lod = r.lod?;
        let z = |v: f64, s: &Stats| (v - s.mean) / s.std;
        Some((z(r.xp, &self.xp).powi(2) + z(r.yp, &self.yp).powi(2) + z(lod, &self.lod).powi(2)).sqrt())
    }
}

/// Empirical quantile with the prototype's convention: v[min(n−1, ⌊p·n⌋)] on a sorted vector.
pub fn quantile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((p * sorted.len() as f64) as usize).min(sorted.len() - 1)]
}

#[derive(Debug, Clone, Serialize)]
pub struct Ladder {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub n: usize,
}

impl Ladder {
    pub fn of(mut v: Vec<f64>) -> Ladder {
        v.retain(|x| x.is_finite());
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Ladder {
            p50: quantile(&v, 0.5),
            p90: quantile(&v, 0.9),
            p99: quantile(&v, 0.99),
            max: v.last().copied().unwrap_or(f64::NAN),
            n: v.len(),
        }
    }
}

/// Rotation rate from the excess length of day (ms): ω_z = Ω₀·86400/(86400 + LOD/1000).
pub fn omega_z(lod_ms: f64) -> f64 {
    OMEGA0 * 86400.0 / (86400.0 + lod_ms / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lstsq_recovers_a_planted_line() {
        let x: Vec<Vec<f64>> = (0..20).map(|i| vec![1.0, i as f64]).collect();
        let y: Vec<f64> = (0..20).map(|i| 3.0 + 0.5 * i as f64).collect();
        let c = lstsq(&x, &y);
        assert!((c[0] - 3.0).abs() < 1e-9 && (c[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn quantile_uses_the_prototype_convention() {
        let v: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        assert_eq!(quantile(&v, 0.5), 6.0); // ⌊0.5·10⌋ = index 5
        assert_eq!(quantile(&v, 0.99), 10.0);
        assert_eq!(quantile(&v, 0.9), 10.0);
    }

    #[test]
    fn a_pure_annual_signal_fits_with_zero_residual() {
        let mjd_now = 60000.0;
        let rows: Vec<Row> = (0..2200)
            .map(|i| {
                let mjd = mjd_now - i as f64;
                let t = mjd - mjd_now;
                let v = 0.1 + 0.05 * (2.0 * std::f64::consts::PI * t / DAYS_YEAR).cos();
                Row { mjd, date: String::new(), flag: 'I', xp: v, xe: None, yp: v, ye: None, ut1utc: None, ue: None, lod: Some(v), le: None }
            })
            .collect();
        let m = Models::build(&rows, mjd_now);
        assert!(m.xp.resid_std < 1e-9, "resid {}", m.xp.resid_std);
        assert!(m.lod.resid_std < 1e-9);
        assert_eq!(m.xp.n, 2192); // (now − 2191.5, now] holds 2192 daily rows
    }
}
