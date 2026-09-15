//! The biosphere channel — Seth Lloyd's question asked of every living thing at once.
//!
//! Lloyd (Nature 406, 2000) bounds what any lump of energy can compute: a system holding
//! energy E performs at most 2E/(πħ) elementary operations per second (Margolus–Levitin),
//! and every irreversible bit costs at least kT·ln2 (Landauer). The biosphere is a lump of
//! energy with a measurable power supply — photosynthesis — and a countable elementary
//! operation — one ATP hydrolysis, the coin every cell spends. So three numbers exist, all
//! derived from published totals, none of them a measurement of "ops":
//!
//!   ops_ml        2·E_biomass/(πħ)   — what physics allows the standing biomass (550 GtC,
//!                                      Bar-On 2018) as a computer; ~1e56 /s
//!   ops_landauer  P_npp/(kT·ln2)      — how many irreversible bit-ops the primary-production
//!                                      power (~1.3e14 W) could pay for at 288 K; ~5e34 /s
//!   ops_atp       P_npp/E_atp         — ATP hydrolyses per second if all fixed energy is
//!                                      respired through ATP at ~50 kJ/mol; ~1.6e33 /s
//!
//! ops_landauer/ops_atp ≈ 30 is the honest headline: life spends about 30 kT·ln2 ≈ 21 kT per
//! elementary operation, a few tens of Landauer — the same ballpark every study of biological
//! computation lands in. The gap to the Margolus–Levitin ceiling is ~23 orders of magnitude.
//!
//! What is LIVE (measured daily): the planet's breathing. Mauna Loa CO₂ falls ~7 ppm every
//! northern summer as the land biosphere draws carbon down and rises again in winter. The
//! daily series is fitted with trend + acceleration + annual + semiannual (the same
//! least-squares machinery the pole uses); the fitted seasonal derivative today, times
//! 2.124 PgC per ppm, is the net biosphere exchange right now. K_bio is the K-family score of
//! that series: |7-day mean residual| / σ of the same statistic over three years, on the
//! < 1 / < 3 / ≥ 3 ladder — how far the biosphere is from the breathing the cycles predicted.
//!
//! Labels: measured (MLO daily CO₂), model (the fit), reference (NPP, biomass, ΔG, ATP),
//! derived (every ops number), assumed (T = 288.15 K; NPP ≈ respiration; one ATP = one op).

use crate::model::{lstsq, mean_std, Ladder, DAYS_YEAR};
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// NOAA GML Mauna Loa daily in-situ CO₂ (ppm), 1974 → two days ago. Free, cited.
pub const SRC_CO2_DAILY: &str = "https://gml.noaa.gov/webdata/ccgg/trends/co2/co2_daily_mlo.txt";

// ── reference constants (each labelled where it is published) ─────────────────────────────
/// Global net primary production, PgC/yr (Field, Behrenfeld, Randerson & Falkowski 1998, Science
/// 281:237 — 56.4 land + 48.5 ocean). Later estimates span 100–120.
pub const NPP_PGC_YR: f64 = 104.9;
/// Total living biomass, GtC (Bar-On, Phillips & Milo 2018, PNAS 115:6506).
pub const BIOMASS_GTC: f64 = 550.0;
/// Free energy stored per mole of carbon fixed: ΔG°(6CO₂+6H₂O→C₆H₁₂O₆+6O₂) = 2 870 kJ/mol ÷ 6.
pub const DG_PER_MOL_C_J: f64 = 478.3e3;
pub const G_PER_MOL_C: f64 = 12.011;
/// In-vivo ΔG of ATP hydrolysis, J/mol (standard −30.5 kJ/mol; cellular −50 to −60).
pub const ATP_J_PER_MOL: f64 = 50.0e3;
/// Base pairs of DNA in the biosphere (Landenmark, Forgan & Cockell 2015, PLoS Biol 13:e1002168).
pub const DNA_BP: f64 = 5.3e37;
pub const AVOGADRO: f64 = 6.02214076e23;
pub const HBAR: f64 = 1.054571817e-34;
pub const KB: f64 = 1.380649e-23;
/// Global mean surface temperature, K (ASSUMED for the Landauer bound).
pub const T_SURFACE_K: f64 = 288.15;
/// PgC per ppm of atmospheric CO₂ (Ballantyne et al. 2012, Nature 488:70).
pub const PGC_PER_PPM: f64 = 2.124;
pub const SEC_YEAR: f64 = 3.15576e7;
/// Fit window and the ladder window, days.
pub const FIT_DAYS: f64 = 6.0 * DAYS_YEAR;
pub const LADDER_DAYS: f64 = 3.0 * DAYS_YEAR;
pub const SMOOTH_DAYS: usize = 7;

#[derive(Debug, Clone, Serialize)]
pub struct Co2Row {
    pub mjd: f64,
    pub date: String,
    pub ppm: f64,
}

/// `year month day decimal ppm` rows; comment lines start with `#`. Missing values are absent
/// rows in this file (NOAA drops days with no valid hourly means), so nothing to filter.
pub fn parse_daily(text: &str) -> Vec<Co2Row> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let (Ok(y), Ok(m), Ok(d), Ok(ppm)) = (f[0].parse::<i64>(), f[1].parse::<u32>(), f[2].parse::<u32>(), f[4].parse::<f64>()) else { continue };
        if !(200.0..700.0).contains(&ppm) {
            continue; // -999.99 sentinels or garbage
        }
        out.push(Co2Row { mjd: crate::time::ymd_to_mjd(y, m, d), date: format!("{y:04}-{m:02}-{d:02}"), ppm });
    }
    out.sort_by(|a, b| a.mjd.partial_cmp(&b.mjd).unwrap());
    out.dedup_by(|a, b| a.mjd == b.mjd);
    out
}

/// 1, t, t², annual, semiannual — t in years from `mjd_now`. The quadratic term carries the
/// growth-rate acceleration (~0.02 ppm/yr²) that a linear trend would fold into the residual
/// over six years.
pub fn basis(t_days: f64) -> Vec<f64> {
    let t = t_days / DAYS_YEAR;
    let w1 = 2.0 * std::f64::consts::PI;
    let w2 = 2.0 * w1;
    vec![1.0, t, t * t, (w1 * t).cos(), (w1 * t).sin(), (w2 * t).cos(), (w2 * t).sin()]
}

#[derive(Debug, Clone, Serialize)]
pub struct BioFit {
    pub coef: Vec<f64>,
    pub resid_std: f64,
    pub n: usize,
    pub model: String,
    pub mjd_now: f64,
}

impl BioFit {
    pub fn build(rows: &[Co2Row], mjd_now: f64) -> Result<BioFit> {
        let w: Vec<&Co2Row> = rows.iter().filter(|r| mjd_now - FIT_DAYS < r.mjd && r.mjd <= mjd_now).collect();
        if w.len() < 365 {
            return Err(anyhow!("only {} daily CO2 rows in the fit window", w.len()));
        }
        let x: Vec<Vec<f64>> = w.iter().map(|r| basis(r.mjd - mjd_now)).collect();
        let y: Vec<f64> = w.iter().map(|r| r.ppm).collect();
        let coef = lstsq(&x, &y);
        let resid: Vec<f64> = x.iter().zip(&y).map(|(xb, yy)| yy - dot(&coef, xb)).collect();
        Ok(BioFit { coef, resid_std: mean_std(&resid).std, n: w.len(), model: "trend + acceleration + annual + semiannual (6 y, daily MLO)".into(), mjd_now })
    }
    pub fn value(&self, mjd: f64) -> f64 {
        dot(&self.coef, &basis(mjd - self.mjd_now))
    }
    /// The seasonal part alone (annual + semiannual), ppm.
    pub fn seasonal(&self, mjd: f64) -> f64 {
        let b = basis(mjd - self.mjd_now);
        self.coef[3] * b[3] + self.coef[4] * b[4] + self.coef[5] * b[5] + self.coef[6] * b[6]
    }
    /// d(seasonal)/dt in ppm per day — the breathing rate; negative = net uptake.
    pub fn seasonal_rate_per_day(&self, mjd: f64) -> f64 {
        let t = (mjd - self.mjd_now) / DAYS_YEAR;
        let w1 = 2.0 * std::f64::consts::PI;
        let w2 = 2.0 * w1;
        let d_per_year = -self.coef[3] * w1 * (w1 * t).sin() + self.coef[4] * w1 * (w1 * t).cos() - self.coef[5] * w2 * (w2 * t).sin() + self.coef[6] * w2 * (w2 * t).cos();
        d_per_year / DAYS_YEAR
    }
    /// Trend rate today, ppm per year (linear + quadratic contribution at t).
    pub fn trend_rate_per_year(&self, mjd: f64) -> f64 {
        let t = (mjd - self.mjd_now) / DAYS_YEAR;
        self.coef[1] + 2.0 * self.coef[2] * t
    }
    /// Peak-to-trough of the seasonal cycle, ppm, and the day-of-year of the maximum.
    pub fn seasonal_amplitude(&self) -> (f64, u32) {
        let mut hi = f64::MIN;
        let mut lo = f64::MAX;
        let mut hi_doy = 0u32;
        let base = crate::time::ymd_to_mjd(2025, 1, 1);
        for d in 0..365u32 {
            let s = self.seasonal(base + d as f64);
            if s > hi { hi = s; hi_doy = d + 1; }
            if s < lo { lo = s; }
        }
        (hi - lo, hi_doy)
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The Lloyd numbers — all derived from the reference constants above, none measured.
#[derive(Debug, Clone, Serialize)]
pub struct Lloyd {
    pub p_npp_w: f64,
    pub e_biomass_j: f64,
    pub e_atp_j: f64,
    pub kt_j: f64,
    pub ops_ml_per_s: f64,
    pub ops_landauer_per_s: f64,
    pub ops_atp_per_s: f64,
    pub ops_atp_per_year: f64,
    pub kt_per_op: f64,
    pub landauer_margin: f64,
    pub orders_below_ml: f64,
    pub memory_bits: f64,
    pub photosynthetic_efficiency: f64,
}

/// Absorbed sunlight, W: S₀·πR²·(1−albedo) with S₀ 1361 W/m², albedo 0.29.
pub const P_SUN_ABSORBED_W: f64 = 1361.0 * std::f64::consts::PI * 6.371e6 * 6.371e6 * (1.0 - 0.29);

pub fn lloyd() -> Lloyd {
    let j_per_g = DG_PER_MOL_C_J / G_PER_MOL_C;
    let p_npp_w = NPP_PGC_YR * 1e15 * j_per_g / SEC_YEAR;
    let e_biomass_j = BIOMASS_GTC * 1e15 * j_per_g;
    let e_atp_j = ATP_J_PER_MOL / AVOGADRO;
    let kt_j = KB * T_SURFACE_K;
    let ops_ml = 2.0 * e_biomass_j / (std::f64::consts::PI * HBAR);
    let ops_landauer = p_npp_w / (kt_j * std::f64::consts::LN_2);
    let ops_atp = p_npp_w / e_atp_j;
    Lloyd {
        p_npp_w,
        e_biomass_j,
        e_atp_j,
        kt_j,
        ops_ml_per_s: ops_ml,
        ops_landauer_per_s: ops_landauer,
        ops_atp_per_s: ops_atp,
        ops_atp_per_year: ops_atp * SEC_YEAR,
        kt_per_op: e_atp_j / kt_j,
        landauer_margin: ops_landauer / ops_atp,
        orders_below_ml: (ops_ml / ops_atp).log10(),
        memory_bits: 2.0 * DNA_BP,
        photosynthetic_efficiency: p_npp_w / P_SUN_ABSORBED_W,
    }
}

/// One published reading of the channel plus the 3-year series for the charts.
pub struct Channel {
    pub latest: Value,
    pub series: Value,
    pub k_bio: f64,
    pub regime: &'static str,
    pub ladder: Ladder,
    pub last_date: String,
}

pub fn channel(rows: &[Co2Row], utc_today_mjd: f64) -> Result<Channel> {
    let last = rows.last().ok_or_else(|| anyhow!("no CO2 rows"))?;
    let fit = BioFit::build(rows, last.mjd)?;
    // residual series over the ladder window, then the 7-day running mean of the residual
    let win: Vec<&Co2Row> = rows.iter().filter(|r| r.mjd > last.mjd - LADDER_DAYS).collect();
    let resid: Vec<f64> = win.iter().map(|r| r.ppm - fit.value(r.mjd)).collect();
    let sm: Vec<Option<f64>> = (0..resid.len())
        .map(|i| if i + 1 >= SMOOTH_DAYS { Some(resid[i + 1 - SMOOTH_DAYS..=i].iter().sum::<f64>() / SMOOTH_DAYS as f64) } else { None })
        .collect();
    let sm_vals: Vec<f64> = sm.iter().flatten().copied().collect();
    let sigma7 = mean_std(&sm_vals).std;
    if !(sigma7 > 0.0) {
        return Err(anyhow!("degenerate residual spread"));
    }
    let z: Vec<Option<f64>> = sm.iter().map(|s| s.map(|v| (v / sigma7).abs())).collect();
    let ladder = Ladder::of(z.iter().flatten().copied().collect());
    let k_bio = z.last().copied().flatten().ok_or_else(|| anyhow!("no smoothed residual today"))?;
    let regime = crate::regime(k_bio);
    let resid_today = *resid.last().unwrap();
    let sm_today = sm.last().copied().flatten().unwrap();
    let rate = fit.seasonal_rate_per_day(last.mjd);
    let (amp, hi_doy) = fit.seasonal_amplitude();
    let max_rate = (0..365).map(|d| fit.seasonal_rate_per_day(crate::time::ymd_to_mjd(2025, 1, 1) + d as f64).abs()).fold(0.0, f64::max);
    let ll = lloyd();
    let stale_days = utc_today_mjd - last.mjd;
    let latest = json!({
        "name": "K_bio — the biosphere's breathing, scored like the pole",
        "source": {"url": SRC_CO2_DAILY, "what": "NOAA GML Mauna Loa in-situ daily CO₂ (ppm)", "cite": "Lan, X., Tans, P. & Thoning, K. W. — NOAA Global Monitoring Laboratory, Trends in CO₂ (gml.noaa.gov/ccgg/trends/)"},
        "today": {
            "date": last.date, "mjd": last.mjd, "ppm": last.ppm, "fit_ppm": fit.value(last.mjd), "seasonal_ppm": fit.seasonal(last.mjd),
            "resid_ppm": resid_today, "resid_7d_ppm": sm_today, "sigma_7d_ppm": sigma7,
            "k_bio": k_bio, "regime": regime, "stale_days": stale_days,
        },
        "breathing": {
            "rate_ppm_per_day": rate, "rate_pgc_per_day": rate * PGC_PER_PPM,
            "direction": if rate < 0.0 { "uptake (northern growing season)" } else { "release (northern autumn/winter)" },
            "index": if max_rate > 0.0 { rate / max_rate } else { 0.0 },
            "amplitude_ppm": amp, "amplitude_pgc": amp * PGC_PER_PPM, "seasonal_max_doy": hi_doy,
            "trend_ppm_per_year": fit.trend_rate_per_year(last.mjd), "trend_pgc_per_year": fit.trend_rate_per_year(last.mjd) * PGC_PER_PPM,
            "note": "the fitted seasonal derivative at Mauna Loa, ×2.124 PgC/ppm; a northern-hemisphere-weighted view of net biosphere exchange, not a global flux inversion",
        },
        "model": {"coef": fit.coef, "resid_std_ppm": fit.resid_std, "n": fit.n, "model": fit.model, "fit_days": FIT_DAYS, "ladder_days": LADDER_DAYS, "smooth_days": SMOOTH_DAYS},
        "ladder": ladder,
        "lloyd": ll,
        "constants": {"npp_pgc_per_year": NPP_PGC_YR, "biomass_gtc": BIOMASS_GTC, "dg_per_mol_c_j": DG_PER_MOL_C_J, "atp_j_per_mol": ATP_J_PER_MOL,
                      "t_surface_k": T_SURFACE_K, "pgc_per_ppm": PGC_PER_PPM, "dna_bp": DNA_BP, "p_sun_absorbed_w": P_SUN_ABSORBED_W},
        "provenance": {
            "measured": ["daily CO₂ mole fraction at Mauna Loa (NOAA GML, in situ)"],
            "model": ["trend + acceleration + annual + semiannual least squares over 6 years", "ladder = empirical quantiles of |7-day residual|/σ over 3 years"],
            "reference": ["NPP 104.9 PgC/yr (Field et al. 1998)", "biomass 550 GtC (Bar-On et al. 2018)", "ΔG 478.3 kJ/mol C", "ATP 50 kJ/mol in vivo", "DNA 5.3e37 bp (Landenmark et al. 2015)", "2.124 PgC/ppm (Ballantyne et al. 2012)"],
            "derived": ["P_npp", "E_biomass", "ops_ml (Margolus–Levitin, 2E/πħ)", "ops_landauer (P/kT ln2)", "ops_atp (P/E_ATP)", "kT per op", "orders below ML", "photosynthetic efficiency"],
            "assumed": ["T = 288.15 K for Landauer", "NPP ≈ global respiration (steady state)", "one ATP hydrolysis = one elementary operation", "all fixed carbon is respired through ATP"],
            "not": ["a measurement of computation", "a global carbon-flux inversion", "a claim that life is a computer in Lloyd's sense — it is Lloyd's bound applied to life's energy budget"],
        },
    });
    let series = json!({
        "meta": {"version": crate::VERSION, "source": SRC_CO2_DAILY, "last_date": last.date, "rows": win.len(), "fields": "mjd date ppm fit seasonal resid resid_7d z"},
        "rows": win.iter().enumerate().map(|(i, r)| json!({
            "mjd": r.mjd, "date": r.date, "ppm": r.ppm, "fit": r4(fit.value(r.mjd)), "seasonal": r4(fit.seasonal(r.mjd)),
            "resid": r4(resid[i]), "resid_7d": sm[i].map(r4).unwrap_or(Value::Null), "z": z[i].map(r4).unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    });
    Ok(Channel { latest, series, k_bio, regime, ladder, last_date: last.date.clone() })
}

fn r4(v: f64) -> Value {
    json!((v * 1e4).round() / 1e4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lloyd_numbers_land_where_the_literature_puts_them() {
        let l = lloyd();
        assert!((l.p_npp_w / 1.32e14 - 1.0).abs() < 0.02, "P_npp {:.3e} W", l.p_npp_w);
        assert!((l.ops_atp_per_s / 1.6e33 - 1.0).abs() < 0.05, "ops_atp {:.3e}", l.ops_atp_per_s);
        assert!((l.ops_landauer_per_s / 4.8e34 - 1.0).abs() < 0.05, "ops_landauer {:.3e}", l.ops_landauer_per_s);
        assert!((l.ops_ml_per_s / 1.3e56 - 1.0).abs() < 0.05, "ops_ml {:.3e}", l.ops_ml_per_s);
        assert!(l.kt_per_op > 15.0 && l.kt_per_op < 30.0, "kT per op {}", l.kt_per_op);
        assert!(l.landauer_margin > 20.0 && l.landauer_margin < 45.0);
        assert!(l.orders_below_ml > 22.0 && l.orders_below_ml < 24.0);
        assert!(l.photosynthetic_efficiency > 0.0005 && l.photosynthetic_efficiency < 0.003, "{}", l.photosynthetic_efficiency);
        assert!((l.memory_bits - 1.06e38).abs() / 1.06e38 < 0.01);
    }

    /// A synthetic Mauna Loa: 2.5 ppm/yr trend with a 0.02 ppm/yr² acceleration, a 3.5 ppm
    /// annual half-amplitude peaking in May, a weak semiannual, and daily noise. The fit must
    /// recover the trend and the seasonal derivative, and the residual must be the noise.
    fn synthetic(days: usize) -> Vec<Co2Row> {
        let base = crate::time::ymd_to_mjd(2019, 1, 1);
        let mut seed = 0x9E3779B97F4A7C15u64;
        (0..days)
            .map(|d| {
                let t = d as f64 / DAYS_YEAR;
                let phase = 2.0 * std::f64::consts::PI * (t - 130.0 / DAYS_YEAR);
                seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
                let noise = ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 1.2;
                let ppm = 410.0 + 2.5 * t + 0.02 * t * t + 3.5 * phase.cos() + 0.6 * (2.0 * phase).cos() + noise;
                let (y, m, dd) = crate::time::mjd_to_ymd(base + d as f64);
                Co2Row { mjd: base + d as f64, date: format!("{y:04}-{m:02}-{dd:02}"), ppm }
            })
            .collect()
    }

    #[test]
    fn fit_recovers_trend_season_and_noise_on_a_synthetic_mauna_loa() {
        let rows = synthetic(7 * 365);
        let last = rows.last().unwrap().mjd;
        let fit = BioFit::build(&rows, last).unwrap();
        let t_end = (7 * 365) as f64 / DAYS_YEAR;
        let expect_rate = 2.5 + 2.0 * 0.02 * t_end;
        assert!((fit.trend_rate_per_year(last) - expect_rate).abs() < 0.15, "trend {} vs {}", fit.trend_rate_per_year(last), expect_rate);
        let (amp, doy) = fit.seasonal_amplitude();
        assert!(amp > 6.0 && amp < 8.5, "amplitude {amp}");
        assert!((100..=160).contains(&doy), "seasonal max doy {doy}");
        assert!(fit.resid_std < 0.45, "residual std {} should be the injected noise (~0.35)", fit.resid_std);
        // the derivative of the seasonal term is the analytic derivative, not a finite difference
        let mjd = last - 100.0;
        let fd = (fit.seasonal(mjd + 0.5) - fit.seasonal(mjd - 0.5)) / 1.0;
        assert!((fit.seasonal_rate_per_day(mjd) - fd).abs() < 1e-6);
    }

    #[test]
    fn channel_scores_a_quiet_biosphere_below_one_and_a_jolt_above_three() {
        let mut rows = synthetic(7 * 365);
        let today = crate::time::now_mjd().floor();
        let quiet = channel(&rows, today).unwrap();
        assert!(quiet.k_bio < 2.0, "quiet k_bio {}", quiet.k_bio);
        assert!(quiet.ladder.p99 > quiet.ladder.p50);
        assert_eq!(quiet.latest["breathing"]["amplitude_ppm"].as_f64().map(|a| a > 6.0), Some(true));
        // a 4 ppm step over the last week — a volcano next to the intake, or a broken sensor
        let n = rows.len();
        for r in rows[n - 7..].iter_mut() {
            r.ppm += 4.0;
        }
        let jolt = channel(&rows, today).unwrap();
        assert!(jolt.k_bio >= 3.0, "jolt k_bio {}", jolt.k_bio);
        assert_eq!(jolt.regime, "critical");
    }

    #[test]
    fn parses_the_noaa_layout_and_drops_sentinels() {
        let text = "# comment\n  1974   5  19  1974.3781    333.46\n  1974   5  20  1974.3808    -999.99\n  2026   9  13  2026.6986    425.04\n";
        let rows = parse_daily(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].date, "2026-09-13");
        assert!((rows[1].ppm - 425.04).abs() < 1e-9);
        assert!(rows[1].mjd > rows[0].mjd);
    }
}
