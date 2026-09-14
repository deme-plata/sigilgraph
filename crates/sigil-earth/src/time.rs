//! Calendar arithmetic without a dependency: MJD ↔ civil date, day-of-year, ISO and HTTP dates.
//! MJD 0 = 1858-11-17; the Unix epoch is MJD 40587.

use std::time::{SystemTime, UNIX_EPOCH};

pub const MJD_UNIX: i64 = 40587;

/// Howard Hinnant's days-from-civil, `z` = days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

pub fn mjd_to_ymd(mjd: f64) -> (i64, u32, u32) {
    civil_from_days(mjd.floor() as i64 - MJD_UNIX)
}

pub fn ymd_to_mjd(y: i64, m: u32, d: u32) -> f64 {
    (days_from_civil(y, m, d) + MJD_UNIX) as f64
}

pub fn date_str(mjd: f64) -> String {
    let (y, m, d) = mjd_to_ymd(mjd);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn doy(y: i64, m: u32, d: u32) -> u32 {
    (days_from_civil(y, m, d) - days_from_civil(y, 1, 1) + 1) as u32
}

pub fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_mjd() -> f64 {
    now_unix() / 86400.0 + MJD_UNIX as f64
}

pub fn iso_utc(unix: f64) -> String {
    let secs = unix.floor() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

pub fn iso_now() -> String {
    iso_utc(now_unix())
}

/// RFC 1123 date for `If-Modified-Since`.
pub fn httpdate(unix: u64) -> String {
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = (unix / 86400) as i64;
    let rem = unix % 86400;
    let (y, m, d) = civil_from_days(days);
    let wd = ((days + 4).rem_euclid(7)) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WD[wd],
        d,
        MO[(m - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mjd_round_trips_and_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(mjd_to_ymd(61296.0), (2026, 9, 13));
        assert_eq!(ymd_to_mjd(2026, 9, 13), 61296.0);
        assert_eq!(doy(2026, 9, 14), 257);
        assert_eq!(doy(2024, 12, 31), 366);
        assert_eq!(date_str(51544.0), "2000-01-01");
        assert_eq!(httpdate(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
    }
}
