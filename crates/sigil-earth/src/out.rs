//! The `fetch` pipeline: sources → model → products → files, then alerts and the attest row.

use crate::eam::{self, Kind};
use crate::eop::{self, Key, Row};
use crate::fetch::{fetch_cached, fetch_first, write_atomic, Source};
use crate::model::{self, Ladder, Models, Reference};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Opts {
    pub out_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub attest_key: PathBuf,
    pub dry_run: bool,
    pub dispatch: crate::alert::Dispatch,
    pub inject_k: Option<f64>,
}

fn r7(v: f64) -> Value {
    json!((v * 1e7).round() / 1e7)
}
fn r7o(v: Option<f64>) -> Value {
    v.map(r7).unwrap_or(Value::Null)
}
fn r6(v: f64) -> Value {
    json!((v * 1e6).round() / 1e6)
}
fn r6o(v: Option<f64>) -> Value {
    v.map(r6).unwrap_or(Value::Null)
}

struct SeriesRow<'a> {
    r: &'a Row,
    k_raw: Option<f64>,
    k_resid: Option<f64>,
    omega_z: Option<f64>,
    model_xp: f64,
    model_yp: f64,
    model_lod: f64,
}

struct Geo {
    mjd: i64,
    date: Option<String>,
    lod_atm_ms: f64,
    lod_ocn_ms: f64,
    lod_hyd_ms: f64,
    lod_geo_ms: f64,
    chi1: [f64; 3],
    chi2: [f64; 3],
    lod_obs_ms: Option<f64>,
    lod_obs_sm31: Option<f64>,
    lod_geo_sm31: Option<f64>,
}

pub fn run(o: &Opts) -> Result<Value> {
    std::fs::create_dir_all(&o.cache_dir)?;
    std::fs::create_dir_all(&o.state_dir)?;
    if !o.dry_run {
        std::fs::create_dir_all(&o.out_dir)?;
    }
    let mut sources: Vec<Value> = Vec::new();
    let note = |sources: &mut Vec<Value>, name: &str, s: &Source, rows: usize| {
        sources.push(json!({"name": name, "source": s, "rows": rows}));
    };

    // ── 1 · IERS finals: history + the daily rapid file, merged ────────────────────────────────
    let all = fetch_cached(eop::SRC_ALL, &o.cache_dir.join("finals2000A.all"), 10_000)?;
    let all_rows = eop::parse_finals(&all.text);
    note(&mut sources, "finals2000A.all", &all.source, all_rows.len());
    let daily_rows = match fetch_first(&[eop::SRC_DAILY.into(), eop::SRC_DAILY_USNO.into()], &o.cache_dir.join("finals2000A.daily"), 100) {
        Ok(d) => {
            let rows = eop::parse_finals(&d.text);
            note(&mut sources, "finals2000A.daily", &d.source, rows.len());
            rows
        }
        Err(e) => {
            eprintln!("finals2000A.daily unavailable: {e}");
            Vec::new()
        }
    };
    let rows = eop::merge(all_rows, daily_rows);
    let obs: Vec<&Row> = rows.iter().filter(|r| r.observed()).collect();
    let last_obs = *obs.last().ok_or_else(|| anyhow!("no observed rows"))?;
    let mjd_now = last_obs.mjd;

    // ── 2 · reference statistics + the de-wobble model ─────────────────────────────────────────
    let raw = Reference::build(&rows, mjd_now);
    let models = Models::build(&rows, mjd_now);

    // ── 3 · the series: three years back, ninety days of prediction ahead ───────────────────────
    let series: Vec<SeriesRow> = rows
        .iter()
        .filter(|r| r.mjd >= mjd_now - 3.0 * model::DAYS_YEAR && r.mjd <= mjd_now + 90.0)
        .map(|r| SeriesRow {
            r,
            k_raw: raw.k_raw(r),
            k_resid: models.k_resid(r),
            omega_z: r.lod.map(model::omega_z),
            model_xp: models.value(Key::Xp, r.mjd),
            model_yp: models.value(Key::Yp, r.mjd),
            model_lod: models.value(Key::Lod, r.mjd),
        })
        .collect();
    let ladder_raw = Ladder::of(series.iter().filter(|s| s.r.observed()).filter_map(|s| s.k_raw).collect());
    let ladder_res = Ladder::of(series.iter().filter(|s| s.r.observed()).filter_map(|s| s.k_resid).collect());
    let today = series.iter().rev().find(|s| s.r.observed() && s.k_raw.is_some()).ok_or_else(|| anyhow!("no observed row with LOD"))?;
    let oz = today.omega_z.unwrap();
    let k_today = o.inject_k.unwrap_or(today.k_resid.unwrap());
    let regime = crate::regime(k_today);
    let utc_today_mjd = crate::time::now_mjd().floor();
    let now_row = rows.iter().find(|r| (r.mjd - utc_today_mjd).abs() < 0.5);

    // ── 4 · GFZ fluids: analyses, attribution, and the forecasts ───────────────────────────────
    let (year, _, _) = crate::time::mjd_to_ymd(mjd_now);
    let mut daily: BTreeMap<Kind, BTreeMap<i64, [f64; 6]>> = BTreeMap::new();
    for k in Kind::ALL {
        let (r, notes) = eam::analysis(k, year, &o.cache_dir);
        for n in notes {
            sources.push(json!({"name": n.name, "source": n.source, "rows": n.rows}));
        }
        daily.insert(k, eam::daily_means(&r));
    }
    let by_mjd: BTreeMap<i64, &SeriesRow> = series.iter().map(|s| (s.r.mjd.round() as i64, s)).collect();
    let mut geo: Vec<Geo> = daily[&Kind::Aam]
        .keys()
        .filter(|m| daily[&Kind::Oam].contains_key(m) && daily[&Kind::Ham].contains_key(m) && (**m as f64) >= mjd_now - 3.0 * model::DAYS_YEAR)
        .map(|m| {
            let (a, oc, h) = (&daily[&Kind::Aam][m], &daily[&Kind::Oam][m], &daily[&Kind::Ham][m]);
            let s = by_mjd.get(m);
            Geo {
                mjd: *m,
                date: s.map(|s| s.r.date.clone()),
                lod_atm_ms: eam::chi3_ms(a),
                lod_ocn_ms: eam::chi3_ms(oc),
                lod_hyd_ms: eam::chi3_ms(h),
                lod_geo_ms: eam::chi3_ms(a) + eam::chi3_ms(oc) + eam::chi3_ms(h),
                chi1: [eam::chi1(a), eam::chi1(oc), eam::chi1(h)],
                chi2: [eam::chi2(a), eam::chi2(oc), eam::chi2(h)],
                lod_obs_ms: s.and_then(|s| if s.r.observed() { s.r.lod } else { None }),
                lod_obs_sm31: None,
                lod_geo_sm31: None,
            }
        })
        .collect();
    let g1: Vec<&Geo> = geo.iter().filter(|g| g.lod_obs_ms.is_some() && (g.mjd as f64) > mjd_now - model::DAYS_YEAR).collect();
    let ys: Vec<f64> = g1.iter().map(|g| g.lod_obs_ms.unwrap()).collect();
    let attr = json!({
        "atm_only": eam::r2(&g1.iter().map(|g| g.lod_atm_ms).collect::<Vec<_>>(), &ys),
        "atm_ocn": eam::r2(&g1.iter().map(|g| g.lod_atm_ms + g.lod_ocn_ms).collect::<Vec<_>>(), &ys),
        "atm_ocn_hyd": eam::r2(&g1.iter().map(|g| g.lod_geo_ms).collect::<Vec<_>>(), &ys),
        "lod_obs_std_ms": model::mean_std(&ys).std,
    });
    let attr_all: eam::Attribution = serde_json::from_value(attr["atm_ocn_hyd"].clone())?;
    // 31-day running means on both sides average the zonal tides out of the observed LOD
    let idx2: Vec<usize> = geo.iter().enumerate().filter(|(_, g)| (g.mjd as f64) > mjd_now - model::DAYS_YEAR - 20.0).map(|(i, _)| i).collect();
    let sm_obs = eam::runmean(&idx2.iter().map(|&i| geo[i].lod_obs_ms).collect::<Vec<_>>(), 31);
    let sm_geo = eam::runmean(&idx2.iter().map(|&i| Some(geo[i].lod_geo_ms)).collect::<Vec<_>>(), 31);
    let sm_atm = eam::runmean(&idx2.iter().map(|&i| Some(geo[i].lod_atm_ms)).collect::<Vec<_>>(), 31);
    for (j, &i) in idx2.iter().enumerate() {
        geo[i].lod_obs_sm31 = sm_obs[j];
        geo[i].lod_geo_sm31 = sm_geo[j];
    }
    let pairs: Vec<(f64, f64, f64)> = idx2
        .iter()
        .enumerate()
        .filter_map(|(j, &i)| match (sm_atm[j], sm_geo[j], sm_obs[j]) {
            (Some(a), Some(g), Some(ob)) if (geo[i].mjd as f64) > mjd_now - model::DAYS_YEAR => Some((a, g, ob)),
            _ => None,
        })
        .collect();
    let attr_sm = json!({
        "atm_only": eam::r2(&pairs.iter().map(|p| p.0).collect::<Vec<_>>(), &pairs.iter().map(|p| p.2).collect::<Vec<_>>()),
        "atm_ocn_hyd": eam::r2(&pairs.iter().map(|p| p.1).collect::<Vec<_>>(), &pairs.iter().map(|p| p.2).collect::<Vec<_>>()),
        "window_days": 31,
        "lod_obs_smoothed_std_ms": model::mean_std(&pairs.iter().map(|p| p.2).collect::<Vec<_>>()).std,
    });
    let gt = geo.last().ok_or_else(|| anyhow!("no excitation rows"))?;
    let gtoday = geo.iter().rev().find(|g| g.lod_obs_ms.is_some());

    // forecasts
    let (ty, tm, td) = crate::time::mjd_to_ymd(utc_today_mjd);
    let doy = crate::time::doy(ty, tm, td);
    let mut fc_daily: BTreeMap<Kind, BTreeMap<i64, [f64; 6]>> = BTreeMap::new();
    let mut fc_issue: BTreeMap<Kind, u32> = BTreeMap::new();
    for k in Kind::ALL {
        if let Some((r, d, n)) = eam::forecast(k, ty, doy, &o.cache_dir) {
            sources.push(json!({"name": n.name, "source": n.source, "rows": n.rows}));
            fc_daily.insert(k, eam::daily_means(&r));
            fc_issue.insert(k, d);
        } else {
            eprintln!("no {} forecast reachable for DOY {doy}", k.upper());
        }
    }
    let eam90 = eam::eam90(ty, doy, &o.cache_dir);
    if let Some((_, _, n)) = &eam90 {
        sources.push(json!({"name": n.name, "source": n.source, "rows": n.rows}));
    }
    let forecast = crate::forecast::build(crate::forecast::Inputs {
        rows: &rows,
        models: &models,
        ladder: &ladder_res,
        attribution: &attr_all,
        today_mjd: today.r.mjd,
        fc_daily: &fc_daily,
        fc_issue: &fc_issue,
        eam90: eam90.as_ref().map(|(r, d, _)| (r.as_slice(), *d)),
        sources: json!(sources.iter().filter(|s| s["name"].as_str().map(|n| n.contains('F')).unwrap_or(false)).collect::<Vec<_>>()),
    });

    // ── 5 · products ──────────────────────────────────────────────────────────────────────────
    let fetched_at = crate::time::iso_now();
    let provenance = json!({
        "measured": ["x_p", "y_p", "UT1-UTC", "LOD (IERS finals2000A, flag I; daily rapid merged)"],
        "predicted": ["rows with flag P (IERS Bulletin A prediction)", "10-day fluids (GFZ ESMGFZ forecast)", "±90 d EAM (GFZ prediction)"],
        "derived": ["omega_x, omega_y, omega_z", "E_rot = ½Iω²", "L = Iω", "pole offset in metres = R⊕·θ", "forecast LOD = −Δ(UT1−UTC)"],
        "model": [format!("K_raw: σ = std of the trailing 365.25 d of observed values"),
                  format!("K_resid: σ = std of residuals after {} (x_p, y_p) and {} (LOD), fitted on 6 years of observed rows", models.xp.model, models.lod.model),
                  "ladder = empirical quantiles of K over the trailing 3 years", "fluids: ECMWF / MPIOM / LSDM effective angular momentum (GFZ), not measurements"],
        "assumed": ["I_earth constant", "components treated as independent in the quadrature sum"],
        "analogy": [],
    });
    let meta = json!({
        "version": crate::VERSION, "fetched_at": fetched_at, "rows_total": rows.len(), "observed_rows": obs.len(),
        "last_observed_date": last_obs.date, "last_observed_mjd": mjd_now, "last_lod_date": today.r.date, "last_lod_mjd": today.r.mjd,
        "utc_today": crate::time::date_str(utc_today_mjd),
        "conventions": "x_p toward the Greenwich meridian, y_p toward 90°W (IERS); in a right-handed X=Greenwich, Y=90°E, Z=north frame ω_y = −Ω0·y_p. LOD = excess length of day over 86400 s, in ms. UT1−UTC in s.",
        "constants": {"OMEGA0_rad_s": model::OMEGA0, "I_earth_kg_m2": model::I_EARTH, "R_earth_m": model::R_EARTH},
        "provenance": provenance, "sources": sources,
        "files": {"series": "/v1/earth/series", "forecast": "/v1/earth/forecast", "excitation": "/v1/earth/excitation", "stream": "/v1/earth/stream"},
    });
    let tr = today.r;
    let today_json = json!({
        "date": tr.date, "mjd": tr.mjd, "xp": tr.xp, "yp": tr.yp, "lod": tr.lod, "ut1utc": tr.ut1utc,
        "k_raw": today.k_raw, "k_resid": k_today, "k_resid_observed": today.k_resid, "regime": regime,
        "injected": o.inject_k.is_some(),
        "omega": [model::OMEGA0 * tr.xp * model::ARCSEC, -model::OMEGA0 * tr.yp * model::ARCSEC, oz],
        "omega_z_minus_nominal": oz - model::OMEGA0,
        "pole_offset_m": model::R_EARTH * tr.xp.hypot(tr.yp) * model::ARCSEC,
        "E_rot_J": 0.5 * model::I_EARTH * oz * oz, "L_kg_m2_s": model::I_EARTH * oz,
        "dE_per_ms_LOD_J": model::I_EARTH * oz * model::OMEGA0 * (1e-3 / 86400.0),
        "equator_speed_m_s": oz * model::R_EARTH,
    });
    let now_json = json!({
        "pole_date": last_obs.date, "pole_mjd": mjd_now, "xp": last_obs.xp, "yp": last_obs.yp,
        "utc_today": crate::time::date_str(utc_today_mjd),
        "ut1utc_today": now_row.and_then(|r| r.ut1utc), "ut1utc_today_flag": now_row.map(|r| r.flag.to_string()),
        "note": "pole_date is the newest observed pole (LOD lags it by 1–3 days); ut1utc_today is the finals row for the current UTC date, flag I or P",
    });
    let ladder = json!({"k_raw": ladder_raw, "k_resid": ladder_res, "chi3_if_gaussian": {"p50": 1.538, "p90": 2.500, "p99": 3.368}});
    let ex_latest = json!({
        "lod_atm_ms": gt.lod_atm_ms, "lod_ocn_ms": gt.lod_ocn_ms, "lod_hyd_ms": gt.lod_hyd_ms, "lod_geo_ms": gt.lod_geo_ms,
        "chi1_atm": gt.chi1[0], "chi2_atm": gt.chi2[0], "chi1_ocn": gt.chi1[1], "chi2_ocn": gt.chi2[1], "chi1_hyd": gt.chi1[2], "chi2_hyd": gt.chi2[2],
    });
    let on_last = gtoday.map(|g| json!({
        "date": g.date, "lod_obs_ms": g.lod_obs_ms, "lod_geo_ms": g.lod_geo_ms, "lod_atm_ms": g.lod_atm_ms, "lod_ocn_ms": g.lod_ocn_ms, "lod_hyd_ms": g.lod_hyd_ms,
        "residual_ms_after_offset": g.lod_obs_ms.unwrap() - (attr_all.offset_ms + attr_all.slope * g.lod_geo_ms),
    }));
    let fc_head: Vec<Value> = forecast.days.iter().take(3).map(|d| json!({"h": d.h, "date": d.date, "k_resid_fcst": d.k_resid_fcst, "k_lo": d.k_lo, "k_hi": d.k_hi, "k_resid_fcst_gfz": d.k_resid_fcst_gfz, "regime_fcst": d.regime_fcst})).collect();
    let chain_path = crate::attest::chain_path(&o.out_dir);
    let existing_alerts = crate::alert::read_rows(&o.out_dir.join("alerts.jsonl"));
    let state_path = o.state_dir.join("alerts-state.json");
    let st = crate::alert::load_state(&state_path);
    let (alert_rows, next_state) = crate::alert::evaluate(k_today, &tr.date, ladder_res.p99, ladder_res.p90, &st, &existing_alerts, o.inject_k.is_some());
    let open_alerts: Vec<Value> = existing_alerts.iter().rev().take(5).map(|r| serde_json::to_value(r).unwrap()).collect();

    let latest = json!({
        "meta": meta, "today": today_json, "now": now_json, "reference_raw": raw,
        "model": {"xp": {"resid_std": models.xp.resid_std, "n": models.xp.n, "model": models.xp.model},
                  "yp": {"resid_std": models.yp.resid_std, "n": models.yp.n, "model": models.yp.model},
                  "lod": {"resid_std": models.lod.resid_std, "n": models.lod.n, "model": models.lod.model}},
        "ladder": ladder,
        "excitation": {"source": "GFZ ESMGFZ effective angular momentum functions v1.0/v1.2 via https://datacenter.iers.org/data/{320,322,321}/ (Dobslaw et al. 2010); 3 h → daily means; GFZ subtracts the 2003–2014 mean",
                       "label": "model (external): ECMWF analysis (atmosphere), MPIOM (ocean), LSDM (land water) — not direct measurements",
                       "latest_mjd": gt.mjd, "latest_date": gt.date, "latest": ex_latest, "on_last_observed_lod_day": on_last,
                       "attribution_365d": attr, "attribution_365d_smoothed31": attr_sm,
                       "note": "attribution_365d: R² over daily values — low because the observed IERS LOD still contains the zonal tides (13.66 d, 27.55 d, ±0.5 ms) that the fluid models do not carry. attribution_365d_smoothed31: the same after a 31-day running mean on both sides, which averages the tides out — the fair number for 'how much of the season-to-season day length is the fluids'. The residual after that is core–mantle coupling, secular tidal braking and model error."},
        "forecast_head": fc_head,
        "tips": crate::tips::with_today(crate::tips::tips(), &crate::tips::TodayCtx {
            date: tr.date.clone(), k_resid: k_today, k_raw: today.k_raw.unwrap_or(f64::NAN), regime: regime.into(),
            p50: ladder_res.p50, p90: ladder_res.p90, p99: ladder_res.p99, lod: tr.lod.unwrap_or(f64::NAN),
            lod_atm: gtoday.map(|g| g.lod_atm_ms), lod_ocn: gtoday.map(|g| g.lod_ocn_ms), lod_hyd: gtoday.map(|g| g.lod_hyd_ms), lod_geo: gtoday.map(|g| g.lod_geo_ms),
            r2_daily: attr["atm_ocn_hyd"]["r2"].as_f64(), r2_sm31: attr_sm["atm_ocn_hyd"]["r2"].as_f64(),
            pole_offset_m: model::R_EARTH * tr.xp.hypot(tr.yp) * model::ARCSEC, xp: tr.xp, yp: tr.yp, ut1utc: tr.ut1utc,
            omega_z_minus: oz - model::OMEGA0, de_per_ms: model::I_EARTH * oz * model::OMEGA0 * (1e-3 / 86400.0),
            fc1_k: forecast.days.first().and_then(|d| d.k_resid_fcst), fc1_k_gfz: forecast.days.first().and_then(|d| d.k_resid_fcst_gfz), fc1_date: forecast.days.first().map(|d| d.date.clone()),
            attest_n: crate::attest::read_chain(&chain_path).iter().rev().find(|r| r.kind == "attest").map(|r| r.n),
            attest_anchored: { let ch = crate::attest::read_chain(&chain_path); let last_n = ch.iter().rev().find(|r| r.kind == "attest").map(|r| r.n); ch.iter().any(|r| r.kind == "anchor" && Some(r.n) == last_n && r.anchor["executed"] == true) },
            armed: next_state.armed,
        }),
        "alerts_recent": open_alerts,
        "attest_last": crate::attest::latest_summary(&crate::attest::read_chain(&chain_path)),
    });
    let series_json = json!({
        "meta": {"version": crate::VERSION, "fetched_at": fetched_at, "last_observed_mjd": mjd_now, "last_lod_mjd": today.r.mjd, "rows": series.len(),
                 "fields": "mjd date flag xp yp lod ut1utc xe ye k_raw k_resid omega_z model_xp model_yp model_lod"},
        "rows": series.iter().map(|s| json!({
            "mjd": s.r.mjd, "date": s.r.date, "flag": s.r.flag.to_string(), "xp": r7(s.r.xp), "yp": r7(s.r.yp), "lod": r7o(s.r.lod), "ut1utc": r7o(s.r.ut1utc),
            "xe": r7o(s.r.xe), "ye": r7o(s.r.ye), "k_raw": r7o(s.k_raw), "k_resid": r7o(s.k_resid), "omega_z": s.omega_z.map(|v| json!(v)).unwrap_or(Value::Null),
            "model_xp": r7(s.model_xp), "model_yp": r7(s.model_yp), "model_lod": r7(s.model_lod),
        })).collect::<Vec<_>>(),
    });
    let excitation_json = json!({
        "meta": {"version": crate::VERSION, "fetched_at": fetched_at, "latest_mjd": gt.mjd, "latest_date": gt.date},
        "latest": ex_latest, "attribution_365d": attr, "attribution_365d_smoothed31": attr_sm,
        "series": geo.iter().filter(|g| (g.mjd as f64) > mjd_now - 400.0).map(|g| json!({
            "mjd": g.mjd, "date": g.date, "lod_atm_ms": r6(g.lod_atm_ms), "lod_ocn_ms": r6(g.lod_ocn_ms), "lod_hyd_ms": r6(g.lod_hyd_ms), "lod_geo_ms": r6(g.lod_geo_ms),
            "chi1_atm": r6(g.chi1[0]), "chi1_ocn": r6(g.chi1[1]), "chi1_hyd": r6(g.chi1[2]), "chi2_atm": r6(g.chi2[0]), "chi2_ocn": r6(g.chi2[1]), "chi2_hyd": r6(g.chi2[2]),
            "lod_obs_ms": r6o(g.lod_obs_ms), "lod_obs_sm31": r6o(g.lod_obs_sm31), "lod_geo_sm31": r6o(g.lod_geo_sm31),
        })).collect::<Vec<_>>(),
    });
    let forecast_json = serde_json::to_value(&forecast)?;

    let summary = json!({
        "ok": true, "dry_run": o.dry_run, "date": tr.date, "pole_date": last_obs.date, "xp": tr.xp, "yp": tr.yp, "lod": tr.lod,
        "k_raw": today.k_raw, "k_resid": k_today, "regime": regime, "ladder_resid_p99": ladder_res.p99, "series_rows": series.len(),
        "excitation_rows": geo.len(), "attr_r2_daily": attr["atm_ocn_hyd"]["r2"], "attr_r2_sm31": attr_sm["atm_ocn_hyd"]["r2"],
        "forecast_days": forecast.days.len(), "gfz_issue": fc_issue.iter().map(|(k, d)| (k.tag(), *d)).collect::<BTreeMap<_, _>>(),
        "eam90_issue": eam90.as_ref().map(|(_, d, _)| *d),
        "sources_live": sources.iter().filter(|s| s["source"]["kind"] != "cached").count(), "sources_total": sources.len(),
        "alerts_new": alert_rows.iter().map(|r| r.kind.clone()).collect::<Vec<_>>(),
    });
    if o.dry_run {
        let mut s = summary;
        s["latest_preview"] = json!({"today": latest["today"], "now": latest["now"], "forecast_head": latest["forecast_head"]});
        s["alerts_preview"] = json!(alert_rows);
        return Ok(s);
    }

    // ── 6 · write (atomic), archive, alerts, attest ───────────────────────────────────────────
    let latest_bytes = serde_json::to_vec(&latest)?;
    write_atomic(&o.out_dir.join("series.json"), &serde_json::to_vec(&series_json)?)?;
    write_atomic(&o.out_dir.join("excitation.json"), &serde_json::to_vec(&excitation_json)?)?;
    write_atomic(&o.out_dir.join("forecast.json"), &serde_json::to_vec(&forecast_json)?)?;
    write_atomic(&o.out_dir.join("provenance.json"), &serde_json::to_vec(&json!({"version": crate::VERSION, "fetched_at": fetched_at, "provenance": provenance, "sources": sources,
        "attest_pubkey": crate::attest::load_or_create_key(&o.attest_key).ok().map(|k| hex::encode(k.verifying_key().to_bytes())),
        "attest_message_v2": "sigil-earth-attest-v2|n|date|blake3|sha256|prev — Ed25519 over the UTF-8 bytes; v1 rows omit sha256",
        "anchor": std::fs::read_to_string(o.state_dir.join("anchor.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()),
        "verify": "sigil-earth verify --remote https://sigilgraph.org  (or the page /kristensen-verify.html)",
        "licence_note": "IERS EOP products and GFZ ESMGFZ EAM products are published for open scientific use; no machine-readable licence is attached to the files. Cite: IERS Rapid Service/Prediction Centre (Bulletin A); Dobslaw et al. 2010, doi:10.1029/2009JB007127."}))?)?;
    write_atomic(&o.out_dir.join("fortolkning.md"), crate::tips::markdown(&latest["tips"], &tr.date).as_bytes())?;
    write_atomic(&o.out_dir.join("latest.json"), &latest_bytes)?;
    // daily archive outside the web root, 60 kept
    let archive = o.state_dir.join("archive");
    std::fs::create_dir_all(&archive)?;
    write_atomic(&archive.join(format!("latest-{}.json", crate::time::date_str(utc_today_mjd))), &latest_bytes)?;
    prune(&archive, "latest-", 60);
    let mut out = summary;
    let mut dispatched = Vec::new();
    for row in &alert_rows {
        crate::alert::append(&o.out_dir.join("alerts.jsonl"), row)?;
        dispatched.push(crate::alert::dispatch(row, o.dispatch));
    }
    crate::alert::save_state(&state_path, &next_state)?;
    out["alerts_dispatch"] = json!(dispatched);
    let key = crate::attest::load_or_create_key(&o.attest_key)?;
    let att = crate::attest::sign_row(&chain_path, &key, &tr.date, &latest_bytes)?;
    out["attest"] = match att {
        Some(r) => json!({"n": r.n, "blake3": r.blake3, "prev": r.prev}),
        None => json!("unchanged"),
    };
    Ok(out)
}

fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with(prefix)).unwrap_or(false)).collect();
    names.sort();
    while names.len() > keep {
        let _ = std::fs::remove_file(names.remove(0));
    }
}
