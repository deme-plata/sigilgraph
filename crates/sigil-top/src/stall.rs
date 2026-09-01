//! Freeze / stall detection: is the chain still advancing? Extracted from main.rs.
//! Fully self-contained (std + its own STALL_* consts).

// A node-top's #1 job is to scream when the chain stops advancing. We persist
// {height, since} to a tiny file so the check works identically across --once (cron),
// --lite (loop) and the TUI: if the polled height hasn't changed for STALL_SECS, the
// node is FROZEN (the exact failure mode that hid the Epsilon QUG freeze behind a green light).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StallState {
    pub(crate) frozen: bool,
    pub(crate) stalled_secs: u64,
}
const STALL_FILE: &str = "/tmp/sigil-top-stall";
const STALL_SECS: u64 = 45;
/// Pure freeze decision, split out from the file I/O so this safety-critical logic
/// (the "scream when the chain stops" net that would have caught the Epsilon freeze
/// hiding behind a green light) is unit-testable — the real path keys off a single
/// fixed /tmp file no test could touch without racing every other test. Given the
/// persisted `(prev_h, since)` and the current `(height, now, online)`, returns the
/// `since` to persist and the verdict. Offline / height-0 = "can't judge": never
/// frozen, clock untouched (erring toward a warning on reconnect, never toward
/// silently swallowing a real freeze).
pub(crate) fn stall_decision(prev_h: u64, since: u64, height: u64, now: u64, online: bool) -> (u64, StallState) {
    if !online || height == 0 {
        return (since, StallState { frozen: false, stalled_secs: 0 });
    }
    // A changed height (or first sight, prev == u64::MAX) resets the clock.
    let since = if height != prev_h { now } else { since };
    let stalled = now.saturating_sub(since);
    (since, StallState { frozen: stalled >= STALL_SECS, stalled_secs: stalled })
}

pub(crate) fn stall_check(height: u64, online: bool) -> StallState {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (mut prev_h, mut since) = (u64::MAX, now);
    if let Ok(s) = std::fs::read_to_string(STALL_FILE) {
        let mut it = s.trim().split(':');
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            prev_h = a.parse().unwrap_or(u64::MAX);
            since = b.parse().unwrap_or(now);
        }
    }
    let (new_since, state) = stall_decision(prev_h, since, height, now, online);
    // Persist only when we actually judged (online, real height) — matches the prior
    // behavior where the offline/height-0 early return never rewrote the file.
    if online && height != 0 {
        let _ = std::fs::write(STALL_FILE, format!("{height}:{new_since}"));
    }
    state
}

#[cfg(test)]
mod stall_decision_tests {
    use super::{stall_decision, STALL_SECS};

    #[test]
    fn offline_or_height_zero_is_never_frozen_and_leaves_the_clock() {
        // since is returned untouched so a reconnect resumes from where it was.
        assert_eq!(stall_decision(100, 1000, 100, 9999, false), (1000, super::StallState { frozen: false, stalled_secs: 0 }));
        assert_eq!(stall_decision(100, 1000, 0, 9999, true), (1000, super::StallState { frozen: false, stalled_secs: 0 }));
    }

    #[test]
    fn first_sight_or_advance_resets_the_clock() {
        // first sight: prev == u64::MAX → since := now, 0 stalled.
        let (since, s) = stall_decision(u64::MAX, 500, 100, 1000, true);
        assert_eq!(since, 1000);
        assert!(!s.frozen && s.stalled_secs == 0);
        // advanced 99 → 100 → since := now.
        let (since, s) = stall_decision(99, 500, 100, 1000, true);
        assert_eq!(since, 1000);
        assert!(!s.frozen);
    }

    #[test]
    fn a_stuck_height_screams_at_the_threshold() {
        // same height held: stalled = now - since; frozen exactly at STALL_SECS.
        let (_, under) = stall_decision(100, 1000, 100, 1000 + STALL_SECS - 1, true);
        assert!(!under.frozen, "one second under the threshold is not yet frozen");
        let (since, at) = stall_decision(100, 1000, 100, 1000 + STALL_SECS, true);
        assert_eq!(since, 1000, "clock is NOT reset while stuck");
        assert!(at.frozen, "at the threshold it must scream");
        assert_eq!(at.stalled_secs, STALL_SECS);
    }
}
