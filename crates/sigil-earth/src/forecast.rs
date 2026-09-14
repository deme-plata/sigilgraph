//! The K⊕ forecast, honestly labelled. Three different kinds of number meet here:
//!   predicted (IERS)  — x_p, y_p, UT1−UTC on the `P` rows of finals2000A (Bulletin A predictions).
//!   derived           — LOD on those days: finals `P` rows carry NO LOD, so it is the finite
//!                       difference of the predicted UT1−UTC, −1000·Δ(UT1−UTC) ms. It contains the
//!                       tides the analysis LOD also contains.
//!   model (GFZ)       — the fluid ΔLOD from the ESMGFZ 10-day forecasts (χ3, atmosphere + ocean +
//!                       land water), calibrated through the 365-day regression against observed LOD.
//!   model (ours)      — the de-wobble model and residual σ, fitted on observed rows only.
//! K_resid_fcst puts the IERS-predicted inputs through our model. K_resid_fcst_gfz replaces the LOD
//! slot with the calibrated fluid forecast — a second, physically independent opinion shown beside
//! the first, never averaged into it.

use crate::eam::{self, Attribution, Kind};
use crate::eop::{Key, Row};
use crate::model::{Ladder, Models};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct Day {
    pub h: i64,
    pub date: String,
    pub mjd: f64,
    pub pole_flag: char,
    pub xp_pred: f64,
    pub yp_pred: f64,
    pub xe: Option<f64>,
    pub ye: Option<f64>,
    pub ut1utc_pred: Option<f64>,
    pub ue: Option<f64>,
    pub lod_pred_ms: Option<f64>,
    pub lod_pred_err_ms: Option<f64>,
    pub lod_atm_fcst_ms: Option<f64>,
    pub lod_ocn_fcst_ms: Option<f64>,
    pub lod_hyd_fcst_ms: Option<f64>,
    pub lod_geo_fcst_ms: Option<f64>,
    pub lod_geo_calibrated_ms: Option<f64>,
    pub residual_ms: Option<f64>,
    pub model_xp: f64,
    pub model_yp: f64,
    pub model_lod: f64,
    pub k_resid_fcst: Option<f64>,
    pub k_lo: Option<f64>,
    pub k_hi: Option<f64>,
    pub k_resid_fcst_gfz: Option<f64>,
    pub regime_fcst: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Eam90Summary {
    pub issue_doy: u32,
    pub rows: usize,
    pub flag_counts: BTreeMap<char, usize>,
    pub daily: Vec<Eam90Day>,
    pub lod_eam_ms_now: Option<f64>,
    pub lod_eam_ms_plus30: Option<f64>,
    pub lod_eam_ms_plus90: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Eam90Day {
    pub mjd: i64,
    pub date: String,
    pub lod_eam_ms: f64,
    pub chi1: f64,
    pub chi2: f64,
    pub flag: char,
}

#[derive(Debug, Clone, Serialize)]
pub struct Forecast {
    pub meta: serde_json::Value,
    pub days: Vec<Day>,
    pub pole_path_pred: Vec<PolePred>,
    pub eam90: Option<Eam90Summary>,
    pub resid_std: BTreeMap<&'static str, f64>,
    pub ladder: Ladder,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolePred {
    pub mjd: f64,
    pub date: String,
    pub xp: f64,
    pub yp: f64,
}

pub struct Inputs<'a> {
    pub rows: &'a [Row],
    pub models: &'a Models,
    pub ladder: &'a Ladder,
    pub attribution: &'a Attribution,
    pub today_mjd: f64,
    pub fc_daily: &'a BTreeMap<Kind, BTreeMap<i64, [f64; 6]>>,
    pub fc_issue: &'a BTreeMap<Kind, u32>,
    pub eam90: Option<(&'a [eam::Eam90Row], u32)>,
    pub sources: serde_json::Value,
}

/// LOD on day d from the UT1−UTC series: −1000·[(UT1−UTC)(d+1) − (UT1−UTC)(d)] ms.
fn lod_from_ut1(by_mjd: &BTreeMap<i64, &Row>, mjd: i64) -> Option<(f64, Option<f64>)> {
    let a = by_mjd.get(&mjd)?.ut1utc?;
    let b = by_mjd.get(&(mjd + 1))?.ut1utc?;
    let err = match (by_mjd.get(&mjd)?.ue, by_mjd.get(&(mjd + 1))?.ue) {
        (Some(e1), Some(e2)) => Some(1000.0 * (e1 * e1 + e2 * e2).sqrt()),
        _ => None,
    };
    Some((-1000.0 * (b - a), err))
}

pub fn build(inp: Inputs<'_>) -> Forecast {
    let by_mjd: BTreeMap<i64, &Row> = inp.rows.iter().map(|r| (r.mjd.round() as i64, r)).collect();
    let m = inp.models;
    let base = inp.today_mjd.round() as i64;
    let mut days = Vec::new();
    for h in 1..=10i64 {
        let mjd = base + h;
        let Some(r) = by_mjd.get(&mjd) else { continue };
        let (lod_pred, lod_err) = match r.lod {
            Some(l) => (Some(l), r.le),
            None => match lod_from_ut1(&by_mjd, mjd) {
                Some((l, e)) => (Some(l), e),
                None => (None, None),
            },
        };
        let fc = |k: Kind| inp.fc_daily.get(&k).and_then(|d| d.get(&mjd)).map(eam::chi3_ms);
        let (atm, ocn, hyd) = (fc(Kind::Aam), fc(Kind::Oam), fc(Kind::Ham));
        let geo = match (atm, ocn, hyd) {
            (Some(a), Some(o), Some(hh)) => Some(a + o + hh),
            _ => None,
        };
        let cal = geo.map(|g| inp.attribution.offset_ms + inp.attribution.slope * g);
        let mjdf = mjd as f64;
        let (mx, my, ml) = (m.value(Key::Xp, mjdf), m.value(Key::Yp, mjdf), m.value(Key::Lod, mjdf));
        let k = lod_pred.map(|l| m.k_resid_at(mjdf, r.xp, r.yp, l));
        // error band: each residual shrunk / grown by the IERS-stated prediction error
        let band = |sign: f64| -> Option<f64> {
            let l = lod_pred?;
            let z = |v: f64, mv: f64, e: Option<f64>, s: f64| ((v - mv).abs() + sign * e.unwrap_or(0.0)).max(0.0) / s;
            Some(
                (z(r.xp, mx, r.xe, m.xp.resid_std).powi(2)
                    + z(r.yp, my, r.ye, m.yp.resid_std).powi(2)
                    + z(l, ml, lod_err, m.lod.resid_std).powi(2))
                .sqrt(),
            )
        };
        let k_gfz = cal.map(|c| m.k_resid_at(mjdf, r.xp, r.yp, c));
        days.push(Day {
            h,
            date: r.date.clone(),
            mjd: mjdf,
            pole_flag: r.flag,
            xp_pred: r.xp,
            yp_pred: r.yp,
            xe: r.xe,
            ye: r.ye,
            ut1utc_pred: r.ut1utc,
            ue: r.ue,
            lod_pred_ms: lod_pred,
            lod_pred_err_ms: lod_err,
            lod_atm_fcst_ms: atm,
            lod_ocn_fcst_ms: ocn,
            lod_hyd_fcst_ms: hyd,
            lod_geo_fcst_ms: geo,
            lod_geo_calibrated_ms: cal,
            residual_ms: match (lod_pred, cal) {
                (Some(l), Some(c)) => Some(l - c),
                _ => None,
            },
            model_xp: mx,
            model_yp: my,
            model_lod: ml,
            k_resid_fcst: k,
            k_lo: band(-1.0),
            k_hi: band(1.0),
            k_resid_fcst_gfz: k_gfz,
            regime_fcst: k.map(crate::regime),
        });
    }
    let pole_path_pred: Vec<PolePred> = inp
        .rows
        .iter()
        .filter(|r| r.flag == 'P' && r.mjd > inp.today_mjd && r.mjd <= inp.today_mjd + 365.0)
        .map(|r| PolePred { mjd: r.mjd, date: r.date.clone(), xp: r.xp, yp: r.yp })
        .collect();
    let eam90 = inp.eam90.map(|(rows, issue)| {
        let mut counts = BTreeMap::new();
        let mut acc: BTreeMap<i64, (usize, f64, f64, f64, char)> = BTreeMap::new();
        for r in rows {
            *counts.entry(r.flag).or_insert(0) += 1;
            let e = acc.entry(r.mjd.floor() as i64).or_insert((0, 0.0, 0.0, 0.0, r.flag));
            e.0 += 1;
            e.1 += r.chi1;
            e.2 += r.chi2;
            e.3 += r.chi3;
            if r.flag == 'P' {
                e.4 = 'P';
            }
        }
        let daily: Vec<Eam90Day> = acc
            .iter()
            .map(|(mjd, (n, c1, c2, c3, f))| Eam90Day {
                mjd: *mjd,
                date: crate::time::date_str(*mjd as f64),
                lod_eam_ms: c3 / *n as f64 * eam::MS,
                chi1: c1 / *n as f64,
                chi2: c2 / *n as f64,
                flag: *f,
            })
            .collect();
        let at = |off: i64| daily.iter().find(|d| d.mjd == base + off).map(|d| d.lod_eam_ms);
        Eam90Summary { issue_doy: issue, rows: rows.len(), flag_counts: counts, lod_eam_ms_now: at(0), lod_eam_ms_plus30: at(30), lod_eam_ms_plus90: at(90), daily }
    });
    let mut resid_std = BTreeMap::new();
    resid_std.insert("xp", m.xp.resid_std);
    resid_std.insert("yp", m.yp.resid_std);
    resid_std.insert("lod", m.lod.resid_std);
    let mut meta = serde_json::json!({
        "issued_at": crate::time::iso_now(),
        "base_date": crate::time::date_str(inp.today_mjd),
        "base_mjd": inp.today_mjd,
        "horizon_days": 10,
        "gfz_issue_doy": inp.fc_issue.iter().map(|(k, d)| (k.tag().to_string(), *d)).collect::<BTreeMap<_, _>>(),
        "definition": "K_resid_fcst uses the SAME de-wobbling model and residual σ as K_resid today (fitted on observed rows only). Pole inputs are IERS predictions (finals P rows). The LOD input is the finite difference of the IERS-predicted UT1−UTC (derived; it contains the zonal tides the analysis LOD also contains). The GFZ column is an independent physical forecast of the fluid part of LOD, calibrated by the 365-day regression against observed LOD, and is shown BESIDE K⊕ as k_resid_fcst_gfz, never inside it. Forecast skill decays after ~3 days.",
        "labels": {
            "xp_pred": "predicted (IERS Bulletin A)", "yp_pred": "predicted (IERS Bulletin A)", "ut1utc_pred": "predicted (IERS Bulletin A)",
            "lod_pred_ms": "derived from IERS-predicted UT1−UTC", "lod_geo_fcst_ms": "model forecast (GFZ ESMGFZ, ECMWF/MPIOM/LSDM 10-day)",
            "lod_geo_calibrated_ms": "model forecast through the 365-day observed-LOD regression",
            "k_resid_fcst": "model (ours) on IERS-predicted inputs", "k_resid_fcst_gfz": "model (ours) with the calibrated GFZ LOD in the LOD slot",
            "k_lo/k_hi": "IERS-stated prediction errors (xe, ye, ue) propagated", "eam90": "model prediction (GFZ, ±90 d, Dill et al. 2018)"
        }
    });
    meta["sources"] = inp.sources;
    Forecast { meta, days, pole_path_pred, eam90, resid_std, ladder: inp.ladder.clone() }
}
