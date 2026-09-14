//! IERS finals2000A fixed-width parser (Bulletin A / rapid service format).
//! Columns (1-based, as in the IERS README): 1-2 yy · 3-4 mm · 5-6 dd · 8-15 MJD · 17 flag
//! (I = IERS observed, P = prediction) · 19-27 x_p (arcsec) · 28-36 err · 38-46 y_p · 47-55 err ·
//! 58 UT1 flag · 59-68 UT1−UTC (s) · 69-78 err · 80-86 LOD (ms) · 87-93 err.

use serde::Serialize;

pub const SRC_ALL: &str = "https://datacenter.iers.org/data/9/finals2000A.all";
pub const SRC_DAILY: &str = "https://datacenter.iers.org/data/13/finals2000A.daily";
pub const SRC_DAILY_USNO: &str = "https://maia.usno.navy.mil/ser7/finals2000A.daily";

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub mjd: f64,
    pub date: String,
    pub flag: char,
    pub xp: f64,
    pub xe: Option<f64>,
    pub yp: f64,
    pub ye: Option<f64>,
    pub ut1utc: Option<f64>,
    pub ue: Option<f64>,
    pub lod: Option<f64>,
    pub le: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Xp,
    Yp,
    Lod,
}

impl Key {
    pub const ALL: [Key; 3] = [Key::Xp, Key::Yp, Key::Lod];
    pub fn name(self) -> &'static str {
        match self {
            Key::Xp => "xp",
            Key::Yp => "yp",
            Key::Lod => "lod",
        }
    }
}

impl Row {
    pub fn get(&self, k: Key) -> Option<f64> {
        match k {
            Key::Xp => Some(self.xp),
            Key::Yp => Some(self.yp),
            Key::Lod => self.lod,
        }
    }
    pub fn observed(&self) -> bool {
        self.flag == 'I'
    }
}

fn fld(l: &[u8], a: usize, b: usize) -> Option<f64> {
    if l.len() < b {
        return None;
    }
    let s = std::str::from_utf8(&l[a..b]).ok()?.trim();
    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

pub fn parse_finals(text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let l = line.as_bytes();
        if l.len() < 70 {
            continue;
        }
        let flag = l[16] as char;
        if flag != 'I' && flag != 'P' {
            continue;
        }
        let num = |a: usize, b: usize| std::str::from_utf8(&l[a..b]).ok().and_then(|s| s.trim().parse::<i64>().ok());
        let (Some(yy), Some(mm), Some(dd)) = (num(0, 2), num(2, 4), num(4, 6)) else { continue };
        let year = if yy >= 73 { 1900 + yy } else { 2000 + yy };
        let (Some(mjd), Some(xp), Some(yp)) = (fld(l, 7, 15), fld(l, 18, 27), fld(l, 37, 46)) else { continue };
        rows.push(Row {
            mjd,
            date: format!("{year:04}-{mm:02}-{dd:02}"),
            flag,
            xp,
            xe: fld(l, 27, 36),
            yp,
            ye: fld(l, 46, 55),
            ut1utc: fld(l, 58, 68),
            ue: if l.len() > 78 { fld(l, 68, 78) } else { None },
            lod: if l.len() > 86 { fld(l, 79, 86) } else { None },
            le: if l.len() > 93 { fld(l, 86, 93) } else { None },
        });
    }
    rows.sort_by(|a, b| a.mjd.partial_cmp(&b.mjd).unwrap());
    rows
}

/// History from `.all`, freshness from `.daily`: a daily row replaces the row with the same MJD.
pub fn merge(all: Vec<Row>, daily: Vec<Row>) -> Vec<Row> {
    let mut map: std::collections::BTreeMap<i64, Row> = all.into_iter().map(|r| ((r.mjd * 1000.0).round() as i64, r)).collect();
    for r in daily {
        map.insert((r.mjd * 1000.0).round() as i64, r);
    }
    map.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "26 913 61296.00 I  0.204163 0.000039  0.312880 0.000030  I 0.0521234 0.0000123  0.7812 0.0031  I   -0.113    0.099    -0.202    0.145                                                               \n\
26 914 61297.00 P  0.203000 0.005000  0.313500 0.005000  P 0.0512000 0.0100000                                                                                                    \n\
short line\n";

    #[test]
    fn parses_observed_and_predicted_rows() {
        let rows = parse_finals(SAMPLE);
        assert_eq!(rows.len(), 2);
        let i = &rows[0];
        assert_eq!(i.date, "2026-09-13");
        assert_eq!(i.flag, 'I');
        assert!((i.xp - 0.204163).abs() < 1e-9);
        assert!((i.yp - 0.312880).abs() < 1e-9);
        assert!((i.ut1utc.unwrap() - 0.0521234).abs() < 1e-9);
        assert!((i.lod.unwrap() - 0.7812).abs() < 1e-9);
        let p = &rows[1];
        assert_eq!(p.flag, 'P');
        assert!(p.lod.is_none());
        assert!(p.ut1utc.is_some());
    }

    #[test]
    fn daily_rows_replace_all_rows_at_the_same_mjd() {
        let all = parse_finals(SAMPLE);
        let daily = parse_finals("26 914 61297.00 I  0.203300 0.000040  0.313600 0.000030  I 0.0511000 0.0000100  0.7900 0.0030                                                               \n");
        let m = merge(all, daily);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].flag, 'I');
        assert!((m[1].lod.unwrap() - 0.79).abs() < 1e-9);
    }
}
