//! Small pure display formatters for the TUI (hex, supply/uptime/eta strings,
//! short roots, progress bars). Extracted from main.rs; `use super::*` reaches
//! the DECIMALS const + the DIM/RESET color codes.
use super::*;

pub(crate) fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub(crate) fn short_hex(b: &[u8]) -> String {
    let h = hex(b);
    if h.len() <= 18 { h } else { format!("{}…{}", &h[..10], &h[h.len() - 6..]) }
}

pub(crate) fn fmt_supply(base: u128) -> String {
    let whole = base / 10u128.pow(DECIMALS);
    // thousands separators
    let s = whole.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.push(','); }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub(crate) fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m {}s", secs % 60) }
}

/// v0.33.3: seconds → compact ETA ("4h 11m", "38m", "2d 3h"). "∞" when not making progress.
pub(crate) fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 { return "—".into(); }
    if secs > 60.0 * 60.0 * 24.0 * 99.0 { return "∞".into(); }
    let s = secs as u64;
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else if m > 0 { format!("{m}m") } else { format!("{s}s") }
}

pub(crate) fn short_root(r: &str) -> String {
    if r.is_empty() { format!("{DIM}—{RESET}") }
    else if r.len() <= 18 { r.to_string() }
    else { format!("{}…{}", &r[..10], &r[r.len() - 6..]) }
}

pub(crate) fn bar(frac: f64, width: usize, color: &str) -> String {
    let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!("{color}{}{DIM}{}{RESET}", "█".repeat(filled), "░".repeat(width - filled))
}

// ── mainnet launch countdown ────────────────────────────────────────────────
/// SIGIL Graph mainnet launch, as a UTC instant.
///
/// **2026-12-18 13:00 CET == 2026-12-18 12:00 UTC.** The single source of truth
/// is `TARGET_MS` in `sigilgraph-org-site/assets/countdown.js`
/// (`Date.parse("2026-12-18T12:00:00Z")`); this is the same moment expressed as
/// unix seconds so the TUI and the website never disagree.
///
/// Stored as an absolute UTC instant on purpose. Building it from local-time
/// parts is how a countdown ends up saying something different in Copenhagen
/// than in New York, which is a bug, not a feature.
pub(crate) const MAINNET_LAUNCH_UNIX: u64 = 1_797_595_200;

/// Human label for the launch instant, matching the site's `TARGET_LABEL`.
pub(crate) const MAINNET_LAUNCH_LABEL: &str = "18 DEC 2026 · 13:00 CET";

/// Time left until mainnet, **without seconds** (operator request): `"105d 20h 07m"`.
///
/// Seconds are deliberately omitted. The TUI repaints on a timer, so a ticking
/// seconds field would either be wrong between frames or force a 1 Hz repaint
/// for a digit nobody reads at a 105-day distance. Minutes are the finest unit
/// that is still honest at this range.
///
/// Under 24 hours it drops the day field (`"20h 07m"`); under one hour it shows
/// minutes alone (`"07m"`). At or past the instant it returns `"LIVE"`.
pub(crate) fn fmt_countdown(now: u64) -> String {
    if now >= MAINNET_LAUNCH_UNIX { return "LIVE".into(); }
    let left = MAINNET_LAUNCH_UNIX - now;
    let (d, h, m) = (left / 86400, (left % 86400) / 3600, (left % 3600) / 60);
    if d > 0 { format!("{d}d {h:02}h {m:02}m") }
    else if h > 0 { format!("{h}h {m:02}m") }
    else { format!("{m}m") }
}

/// `fmt_countdown` against the wall clock. Falls back to the launch label's
/// own instant (yielding "LIVE") only if the system clock is before the epoch,
/// which cannot happen in practice but must not panic if it does.
pub(crate) fn countdown_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(MAINNET_LAUNCH_UNIX);
    fmt_countdown(now)
}

#[cfg(test)]
mod countdown_tests {
    use super::*;

    #[test]
    fn launch_instant_matches_the_website() {
        // 2026-12-18T12:00:00Z. If this ever disagrees with countdown.js the
        // TUI and the site show different launches, which is the one bug this
        // constant exists to prevent.
        assert_eq!(MAINNET_LAUNCH_UNIX, 1_797_595_200);
        // Sanity: it is 13:00 CET, i.e. 12:00 UTC, i.e. noon on a UTC day boundary.
        assert_eq!(MAINNET_LAUNCH_UNIX % 86_400, 43_200, "must be 12:00:00 UTC");
    }

    #[test]
    fn no_seconds_anywhere_in_the_output() {
        // The operator's actual requirement: no seconds field.
        for t in [0u64, 1_788_450_755, MAINNET_LAUNCH_UNIX - 1, MAINNET_LAUNCH_UNIX - 59] {
            let s = fmt_countdown(t);
            assert!(!s.contains('s'), "seconds leaked into {s:?}");
        }
    }

    #[test]
    fn counts_down_in_days_hours_minutes() {
        let t = MAINNET_LAUNCH_UNIX - (105 * 86400 + 20 * 3600 + 7 * 60 + 42);
        assert_eq!(fmt_countdown(t), "105d 20h 07m", "42 leftover seconds must be dropped, not rounded up");
    }

    #[test]
    fn drops_empty_leading_fields() {
        assert_eq!(fmt_countdown(MAINNET_LAUNCH_UNIX - (20 * 3600 + 7 * 60)), "20h 07m");
        assert_eq!(fmt_countdown(MAINNET_LAUNCH_UNIX - 7 * 60), "7m");
        // Under a minute is still "0m", never a seconds countdown.
        assert_eq!(fmt_countdown(MAINNET_LAUNCH_UNIX - 30), "0m");
    }

    #[test]
    fn reads_live_at_and_after_the_instant() {
        assert_eq!(fmt_countdown(MAINNET_LAUNCH_UNIX), "LIVE");
        assert_eq!(fmt_countdown(MAINNET_LAUNCH_UNIX + 10_000_000), "LIVE");
    }

    #[test]
    fn hours_and_minutes_are_zero_padded_but_days_are_not() {
        // "105d 20h 07m" — padding keeps the header from reflowing as digits
        // drop, which is why h/m are padded and d (which only shrinks) is not.
        let s = fmt_countdown(MAINNET_LAUNCH_UNIX - (3 * 86400 + 4 * 3600 + 5 * 60));
        assert_eq!(s, "3d 04h 05m");
    }

    #[test]
    fn label_agrees_with_the_constant() {
        assert!(MAINNET_LAUNCH_LABEL.contains("18 DEC 2026"));
        assert!(MAINNET_LAUNCH_LABEL.contains("13:00 CET"));
    }
}
