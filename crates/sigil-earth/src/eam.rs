//! GFZ ESMGFZ effective angular momentum functions: atmosphere (AAM, ECMWF), ocean (OAM, MPIOM),
//! land hydrology (HAM, LSDM). χ1, χ2 excite polar motion, χ3 the length of day:
//! ΔLOD_geo = 86400 s · Σχ3 (ms: × 86400e3). GFZ subtracts the 2003–2014 mean. These are MODEL
//! products (external), never measurements — every consumer labels them so.
//!
//! Analyses come from the IERS datacenter mirror (one file per year, 3-hourly / daily rows, 11
//! columns: `YYYY MM DD HH MJD mass_x mass_y mass_z motion_x motion_y motion_z`). Forecasts come
//! from GFZ's own host (10 days ahead, same layout, one file per issue day `<YYYY>_<DOY>F`), plus
//! the ±90-day combined EAM prediction (5 columns: `MJD x y z FLAG`, FLAG ∈ C/A/P).

use crate::fetch::{fetch_cached, Fetched, Source};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

pub const MS: f64 = 86400.0 * 1000.0;
pub const GFZ_BASE: &str = "https://rz-vm480.gfz.de/files/ESMGFZ/EAM";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Aam,
    Oam,
    Ham,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Aam, Kind::Oam, Kind::Ham];
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Aam => "atm",
            Kind::Oam => "ocn",
            Kind::Ham => "hyd",
        }
    }
    pub fn upper(self) -> &'static str {
        match self {
            Kind::Aam => "AAM",
            Kind::Oam => "OAM",
            Kind::Ham => "HAM",
        }
    }
    fn iers_id(self) -> u32 {
        match self {
            Kind::Aam => 320,
            Kind::Oam => 322,
            Kind::Ham => 321,
        }
    }
    fn stem(self) -> &'static str {
        match self {
            Kind::Aam => "ESMGFZ_AAM_v1.0_03h",
            Kind::Oam => "ESMGFZ_OAM_v1.0_03h",
            Kind::Ham => "ESMGFZ_HAM_v1.2_24h",
        }
    }
    pub fn analysis_file(self, year: i64) -> String {
        format!("{}_{year}.asc", self.stem())
    }
    pub fn analysis_url(self, year: i64) -> String {
        format!("https://datacenter.iers.org/data/{}/{}", self.iers_id(), self.analysis_file(year))
    }
    pub fn forecast_file(self, year: i64, doy: u32) -> String {
        format!("{}_{year}_{doy:03}F.asc", self.stem())
    }
    pub fn forecast_url(self, year: i64, doy: u32) -> String {
        format!("{GFZ_BASE}/operational_{}_forecast/{}", self.upper(), self.forecast_file(year, doy))
    }
}

/// One 11-column row: MJD + [mass_x, mass_y, mass_z, motion_x, motion_y, motion_z].
pub type Row = (f64, [f64; 6]);

pub fn parse_eam11(text: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for line in text.lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() != 11 || p[0].len() != 4 || !p[0].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let vals: Option<Vec<f64>> = p[4..11].iter().map(|s| s.parse::<f64>().ok()).collect();
        if let Some(v) = vals {
            out.push((v[0], [v[1], v[2], v[3], v[4], v[5], v[6]]));
        }
    }
    out
}

/// One 5-column EAM-90d row: MJD, χ1, χ2, χ3, flag (C = combined analysis, A = analysis, P = prediction).
#[derive(Debug, Clone, Serialize)]
pub struct Eam90Row {
    pub mjd: f64,
    pub chi1: f64,
    pub chi2: f64,
    pub chi3: f64,
    pub flag: char,
}

pub fn parse_eam90(text: &str) -> Vec<Eam90Row> {
    let mut out = Vec::new();
    for line in text.lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() != 5 || p[4].len() != 1 {
            continue;
        }
        let flag = p[4].chars().next().unwrap();
        if !"CAP".contains(flag) {
            continue;
        }
        let v: Option<Vec<f64>> = p[..4].iter().map(|s| s.parse::<f64>().ok()).collect();
        if let Some(v) = v {
            out.push(Eam90Row { mjd: v[0], chi1: v[1], chi2: v[2], chi3: v[3], flag });
        }
    }
    out
}

/// 3-hourly (or daily) rows → daily means keyed by ⌊MJD⌋.
pub fn daily_means(rows: &[Row]) -> BTreeMap<i64, [f64; 6]> {
    let mut acc: BTreeMap<i64, (usize, [f64; 6])> = BTreeMap::new();
    for (mjd, v) in rows {
        let e = acc.entry(mjd.floor() as i64).or_insert((0, [0.0; 6]));
        e.0 += 1;
        for i in 0..6 {
            e.1[i] += v[i];
        }
    }
    acc.into_iter()
        .map(|(k, (n, s))| {
            let mut m = [0.0; 6];
            for i in 0..6 {
                m[i] = s[i] / n as f64;
            }
            (k, m)
        })
        .collect()
}

pub fn chi3_ms(v: &[f64; 6]) -> f64 {
    (v[2] + v[5]) * MS
}
pub fn chi1(v: &[f64; 6]) -> f64 {
    v[0] + v[3]
}
pub fn chi2(v: &[f64; 6]) -> f64 {
    v[1] + v[4]
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceNote {
    pub name: String,
    pub source: Source,
    pub rows: usize,
}

/// Analysis rows for `year` and `year − 1` from the IERS mirror (cached per file).
pub fn analysis(kind: Kind, year: i64, cache_dir: &Path) -> (Vec<Row>, Vec<SourceNote>) {
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for y in [year, year - 1] {
        let fname = kind.analysis_file(y);
        match fetch_cached(&kind.analysis_url(y), &cache_dir.join(&fname), 50) {
            Ok(Fetched { text, source }) => {
                let r = parse_eam11(&text);
                notes.push(SourceNote { name: fname, source, rows: r.len() });
                rows.extend(r);
            }
            Err(e) => eprintln!("eam {}: {e}", fname),
        }
    }
    (rows, notes)
}

/// The newest reachable 10-day forecast: today's DOY first, then the two previous issues.
pub fn forecast(kind: Kind, year: i64, doy: u32, cache_dir: &Path) -> Option<(Vec<Row>, u32, SourceNote)> {
    for back in 0..3u32 {
        let (y, d) = roll_back(year, doy, back);
        let fname = kind.forecast_file(y, d);
        let cache = cache_dir.join(&fname);
        // an issue file never changes after publication: a cached copy is as good as live
        if let Ok(text) = std::fs::read_to_string(&cache) {
            let r = parse_eam11(&text);
            let n = r.len();
            if n >= 8 {
                return Some((r, d, SourceNote { name: fname.clone(), source: Source::NotModified { url: kind.forecast_url(y, d) }, rows: n }));
            }
        }
        match fetch_cached(&kind.forecast_url(y, d), &cache, 8) {
            Ok(Fetched { text, source }) if source.is_live() => {
                let r = parse_eam11(&text);
                let n = r.len();
                if n >= 8 {
                    return Some((r, d, SourceNote { name: fname, source, rows: n }));
                }
            }
            _ => {}
        }
    }
    None
}

/// The ±90-day combined EAM prediction (newest issue: tomorrow's DOY is sometimes already up).
pub fn eam90(year: i64, doy: u32, cache_dir: &Path) -> Option<(Vec<Eam90Row>, u32, SourceNote)> {
    let candidates = [(year, doy + 1), (year, doy), roll_back(year, doy, 1), roll_back(year, doy, 2)];
    for (y, d) in candidates {
        let fname = format!("ESMGFZ_EAM-90d_03h_{y}_{d:03}F.asc");
        let url = format!("{GFZ_BASE}/operational_EAM_90d_prediction/{fname}");
        let cache = cache_dir.join(&fname);
        if let Ok(text) = std::fs::read_to_string(&cache) {
            let r = parse_eam90(&text);
            let n = r.len();
            if n >= 100 {
                return Some((r, d, SourceNote { name: fname, source: Source::NotModified { url }, rows: n }));
            }
        }
        match fetch_cached(&url, &cache, 100) {
            Ok(Fetched { text, source }) if source.is_live() => {
                let r = parse_eam90(&text);
                let n = r.len();
                if n >= 100 {
                    return Some((r, d, SourceNote { name: fname, source, rows: n }));
                }
            }
            _ => {}
        }
    }
    None
}

fn roll_back(year: i64, doy: u32, back: u32) -> (i64, u32) {
    if doy > back {
        (year, doy - back)
    } else {
        let prev_len = if (year - 1) % 4 == 0 && ((year - 1) % 100 != 0 || (year - 1) % 400 == 0) { 366 } else { 365 };
        (year - 1, prev_len + doy - back)
    }
}

/// Least-squares line y = c + b·x with R² and RMS residual — the LOD attribution.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct Attribution {
    pub slope: f64,
    pub offset_ms: f64,
    pub r2: f64,
    pub rms_resid_ms: f64,
    pub n: usize,
}

pub fn r2(xs: &[f64], ys: &[f64]) -> Attribution {
    let n = xs.len();
    if n == 0 {
        return Attribution::default();
    }
    let nf = n as f64;
    let mx = xs.iter().sum::<f64>() / nf;
    let my = ys.iter().sum::<f64>() / nf;
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    let syy: f64 = ys.iter().map(|y| (y - my).powi(2)).sum();
    let b = if sxx != 0.0 { sxy / sxx } else { 0.0 };
    let c = my - b * mx;
    let rms = (xs.iter().zip(ys).map(|(x, y)| (y - (c + b * x)).powi(2)).sum::<f64>() / nf).sqrt();
    Attribution { slope: b, offset_ms: c, r2: if sxx != 0.0 && syy != 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 }, rms_resid_ms: rms, n }
}

/// Centred running mean over `w` samples, skipping gaps; None where fewer than w/2 values exist.
pub fn runmean(vals: &[Option<f64>], w: usize) -> Vec<Option<f64>> {
    let h = w / 2;
    (0..vals.len())
        .map(|i| {
            let lo = i.saturating_sub(h);
            let hi = (i + h + 1).min(vals.len());
            let seg: Vec<f64> = vals[lo..hi].iter().flatten().copied().collect();
            if seg.len() >= w / 2 {
                Some(seg.iter().sum::<f64>() / seg.len() as f64)
            } else {
                None
            }
        })
        .collect()
}

pub fn year_of_file(rows: &[Row]) -> Option<f64> {
    rows.last().map(|r| r.0)
}

/// Result type alias kept for callers that want `?` on the pieces above.
pub type EamResult<T> = Result<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_11_column_layout_and_the_5_column_prediction() {
        let a = "# header\n2026 09 13 21  61296.875  1.1e-8 2.2e-8 3.3e-8 4.4e-9 5.5e-9 6.6e-9\n";
        let r = parse_eam11(a);
        assert_eq!(r.len(), 1);
        assert!((r[0].0 - 61296.875).abs() < 1e-9);
        assert!((chi3_ms(&r[0].1) - (3.3e-8 + 6.6e-9) * MS).abs() < 1e-9);
        let p = "61296.000 1.0e-8 2.0e-8 3.0e-8 P\nbad line\n61296.125 1.0e-8 2.0e-8 4.0e-8 C\n";
        let q = parse_eam90(p);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].flag, 'P');
    }

    #[test]
    fn daily_means_average_the_3h_rows() {
        let rows: Vec<Row> = (0..8).map(|i| (61296.0 + i as f64 * 0.125, [i as f64; 6])).collect();
        let d = daily_means(&rows);
        assert_eq!(d.len(), 1);
        assert!((d[&61296][0] - 3.5).abs() < 1e-12);
    }

    #[test]
    fn r2_of_a_line_is_one_and_runmean_skips_gaps() {
        let xs: Vec<f64> = (0..10).map(|i| i as f64).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 0.5 + 2.0 * x).collect();
        let a = r2(&xs, &ys);
        assert!((a.r2 - 1.0).abs() < 1e-12 && (a.slope - 2.0).abs() < 1e-12);
        let v: Vec<Option<f64>> = (0..40).map(|i| if i % 7 == 0 { None } else { Some(1.0) }).collect();
        let m = runmean(&v, 31);
        assert!(m[20].map(|x| (x - 1.0).abs() < 1e-12).unwrap_or(false));
        assert_eq!(roll_back(2026, 1, 1), (2025, 365));
        assert_eq!(roll_back(2025, 2, 1), (2025, 1));
    }
}
