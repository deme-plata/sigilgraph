//! The node must never fill the disk it lives on.
//!
//! A chain only grows. A node that keeps writing until the filesystem reads 0 bytes free takes every other program
//! on that disk down with it, and its own last write is the one that gets cut in half. Measured on Epsilon: the 40 GB
//! system disk hit 0 B free on 2026-09-15 (three times), 2026-10-16/17 (twice) and 2026-11-09 (twice), each time from
//! a sigil-top started with its home on that disk.
//!
//! Two rules, both about the filesystem that holds the node's data folder:
//!   * a node does not START with less than the start floor free (default 4 GiB, `SIGIL_MIN_FREE_GB`);
//!   * a RUNNING node stops itself cleanly (state snapshot written, like a normal stop) below 1 GiB free.
//!
//! `SIGIL_MIN_FREE_GB=0` turns both off. A node configured by its environment (an operator's block-maker) is left
//! alone unless its operator sets `SIGIL_MIN_FREE_GB`: a block-maker stopping itself is the operator's decision.
//! When free space cannot be measured the guard does nothing — it must never be the reason a node does not run.

use std::path::Path;

pub(crate) const GIB: u64 = 1 << 30;
/// Below this much free space a node does not start (unless `SIGIL_MIN_FREE_GB` says otherwise).
pub(crate) const DEFAULT_START_FLOOR: u64 = 4 * GIB;
/// Below this a running node stops. Enough room left for its own shutdown snapshot and for the rest of the machine.
pub(crate) const STOP_FLOOR: u64 = GIB;
pub(crate) const ENV: &str = "SIGIL_MIN_FREE_GB";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The guard is off, or free space could not be measured.
    NotJudged,
    Room,
    /// Under the start floor: do not start; a running node keeps going and says so.
    Low,
    /// Under the stop floor: a running node stops now.
    Critical,
}

/// The start floor in bytes from the setting's text; `None` = the guard is off (`0`).
/// Unset, empty or unreadable text means the default — a typo must not switch the guard off.
pub(crate) fn start_floor(setting: Option<&str>) -> Option<u64> {
    match setting.map(str::trim).filter(|s| !s.is_empty()).map(str::parse::<f64>) {
        Some(Ok(gb)) if gb == 0.0 => None,
        Some(Ok(gb)) if gb.is_finite() && gb > 0.0 => Some((gb * GIB as f64) as u64),
        _ => Some(DEFAULT_START_FLOOR),
    }
}

/// Does the guard apply to this node? A user's node: always. An operator-configured one: only when asked.
pub(crate) fn applies(user_mode: bool, setting: Option<&str>) -> bool {
    user_mode || setting.is_some_and(|s| !s.trim().is_empty())
}

pub(crate) fn judge(free: Option<u64>, floor: Option<u64>) -> Verdict {
    let (Some(free), Some(floor)) = (free, floor) else { return Verdict::NotJudged };
    if free < STOP_FLOOR.min(floor) {
        Verdict::Critical
    } else if free < floor {
        Verdict::Low
    } else {
        Verdict::Room
    }
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}

/// What the user reads when a node does not start. Says the number, the folder and the two ways out.
pub(crate) fn refusal(free: u64, floor: u64, dir: &Path) -> String {
    format!(
        "not starting the node: {:.2} GB free on the disk that holds {} (it needs {:.2} GB; a full disk stops other \
         programs too). Free some space, or keep the chain on a bigger disk: set SIGIL_TOP_DB=<big disk>/sigil-top-blocks.db \
         and start again. ({ENV}=<GB> changes the limit, 0 turns it off.)",
        gb(free),
        dir.display(),
        gb(floor),
    )
}

/// What the log says when a running node stops itself.
pub(crate) fn stop_notice(free: u64, dir: &Path) -> String {
    format!(
        "stopping the node: only {:.2} GB free on the disk that holds {} — it saves its state and stops before the \
         disk is full. Free some space (or move the chain: SIGIL_TOP_DB) and start again.",
        gb(free),
        dir.display(),
    )
}

/// One check for a node about to start. `Err(message)` = do not start.
pub(crate) fn check_start(dir: &Path, user_mode: bool) -> Result<(), String> {
    let setting = std::env::var(ENV).ok();
    if !applies(user_mode, setting.as_deref()) {
        return Ok(());
    }
    let floor = start_floor(setting.as_deref());
    let free = free_bytes(dir);
    match (judge(free, floor), free, floor) {
        (Verdict::Low | Verdict::Critical, Some(free), Some(floor)) => Err(refusal(free, floor, dir)),
        _ => Ok(()),
    }
}

/// Watch the disk while the node runs in this process: a line when space gets low (at most every 10 minutes), and
/// `on_critical` once when it falls under the stop floor. Does nothing when the guard does not apply.
pub(crate) fn watch(dir: std::path::PathBuf, user_mode: bool, on_critical: impl Fn() + Send + 'static) {
    let setting = std::env::var(ENV).ok();
    if !applies(user_mode, setting.as_deref()) {
        return;
    }
    let Some(floor) = start_floor(setting.as_deref()) else { return };
    let _ = std::thread::Builder::new().name("own-node-disk-guard".into()).spawn(move || {
        let mut last_warned: Option<std::time::Instant> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let free = free_bytes(&dir);
            match (judge(free, Some(floor)), free) {
                (Verdict::Critical, Some(free)) => {
                    let msg = stop_notice(free, &dir);
                    eprintln!("🛑 sigil-top: {msg}");
                    crate::boot_trace(&format!("disk guard: {msg}"));
                    on_critical();
                    return;
                }
                (Verdict::Low, Some(free)) if last_warned.is_none_or(|t| t.elapsed().as_secs() >= 600) => {
                    last_warned = Some(std::time::Instant::now());
                    eprintln!(
                        "⚠ sigil-top: {:.1} GB free on the disk that holds {} — the node stops itself below {:.0} GB",
                        gb(free),
                        dir.display(),
                        gb(STOP_FLOOR.min(floor)),
                    );
                }
                _ => {}
            }
        }
    });
}

/// Free bytes on the filesystem holding `path` (or its nearest existing ancestor), as an unprivileged program may
/// use them. `None` when it cannot be determined — callers must treat that as "do not act".
#[cfg(not(windows))]
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let out = std::process::Command::new("df").arg("-Pk").arg(p).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    line.split_whitespace().nth(3)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// Windows has no `df`; ask the system directly (bytes available to this user, quotas included).
#[cfg(windows)]
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut p = path;
    while !p.is_dir() {
        p = p.parent()?;
    }
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut avail: u64 = 0;
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; `avail` is a valid u64; the two
    // optional out-pointers are null, which the API allows.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(avail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_read_and_a_typo_never_switches_the_guard_off() {
        assert_eq!(start_floor(None), Some(DEFAULT_START_FLOOR));
        assert_eq!(start_floor(Some("")), Some(DEFAULT_START_FLOOR));
        assert_eq!(start_floor(Some("ten")), Some(DEFAULT_START_FLOOR));
        assert_eq!(start_floor(Some("-3")), Some(DEFAULT_START_FLOOR));
        assert_eq!(start_floor(Some("0")), None);
        assert_eq!(start_floor(Some(" 10 ")), Some(10 * GIB));
        assert_eq!(start_floor(Some("0.5")), Some(GIB / 2));
    }

    #[test]
    fn a_users_node_is_always_guarded_and_an_operators_only_when_asked() {
        assert!(applies(true, None));
        assert!(applies(true, Some("0"))); // applies, and then the 0 switches it off
        assert!(!applies(false, None));
        assert!(!applies(false, Some("  ")));
        assert!(applies(false, Some("8")));
    }

    #[test]
    fn verdicts_follow_the_two_floors() {
        let floor = Some(DEFAULT_START_FLOOR);
        assert_eq!(judge(Some(50 * GIB), floor), Verdict::Room);
        assert_eq!(judge(Some(DEFAULT_START_FLOOR), floor), Verdict::Room);
        assert_eq!(judge(Some(DEFAULT_START_FLOOR - 1), floor), Verdict::Low);
        assert_eq!(judge(Some(STOP_FLOOR), floor), Verdict::Low);
        assert_eq!(judge(Some(STOP_FLOOR - 1), floor), Verdict::Critical);
        assert_eq!(judge(Some(0), floor), Verdict::Critical);
        // unmeasurable or switched off: the guard never acts
        assert_eq!(judge(None, floor), Verdict::NotJudged);
        assert_eq!(judge(Some(0), None), Verdict::NotJudged);
        // a floor set under 1 GiB lowers the stop line with it (a small test disk must still be usable)
        assert_eq!(judge(Some(GIB / 4 + 1), Some(GIB / 4)), Verdict::Room);
        assert_eq!(judge(Some(GIB / 4 - 1), Some(GIB / 4)), Verdict::Critical);
    }

    #[test]
    fn the_refusal_names_the_number_the_folder_and_the_way_out() {
        let m = refusal(3 * GIB / 2, DEFAULT_START_FLOOR, Path::new("/root/sigil-top-node-sigil-g3"));
        assert!(m.contains("1.50 GB free"), "{m}");
        assert!(m.contains("/root/sigil-top-node-sigil-g3"), "{m}");
        assert!(m.contains("needs 4.00 GB"), "{m}");
        assert!(m.contains("SIGIL_TOP_DB="), "{m}");
        assert!(m.contains("SIGIL_MIN_FREE_GB"), "{m}");
    }

    #[test]
    fn free_space_is_measured_for_a_folder_that_does_not_exist_yet() {
        let tmp = std::env::temp_dir();
        let here = free_bytes(&tmp).expect("free space of the temp folder");
        let deeper = free_bytes(&tmp.join("sigil-disk-guard-no-such-folder").join("x")).expect("nearest ancestor");
        assert!(here > 0);
        // same filesystem, read a moment apart: within 1 GiB of each other
        assert!(here.abs_diff(deeper) < GIB, "{here} vs {deeper}");
    }
}
