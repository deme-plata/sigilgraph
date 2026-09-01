//! Boot-health / crash-recovery: strike counting, the healthy-boot marker, the
//! self-heal timer, and the operator `revert` command. Extracted from main.rs.
//! `use super::*` reaches main.rs's VERSION / HEAL_SECS / colors / self-update
//! helpers — the heroes.rs/wallet_ui.rs module pattern.
#![allow(clippy::items_after_test_module)]
use super::*;

pub(crate) fn prev_binary_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(if cfg!(windows) { "sigil-top-prev.exe" } else { "sigil-top-prev" }))
}
pub(crate) fn boot_marker_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(".sigil-top-boot"))
}
/// Record a boot attempt for the running VERSION; return the consecutive-unhealed strike count
/// (1 on a fresh version). Best-effort — any IO failure returns 1 (proceed, just no rollback).
/// Given the current boot-marker contents (if any) and this binary's version, return
/// the consecutive-unhealed strike count for the running version. A marker for the SAME
/// version increments; a marker for a DIFFERENT version, garbage, or no marker resets to
/// 1 — a fresh binary must start its own count, or an update would inherit the old
/// version's strikes and could self-revert on its very first boot. Pure; the caller does
/// the I/O, so this crash-recovery decision is unit-testable in isolation.
pub(crate) fn next_strike_count(marker: Option<&str>, current_version: &str) -> u32 {
    match marker.map(str::trim).and_then(|s| s.split_once(':')) {
        Some((ver, n)) if ver == current_version => n.parse::<u32>().unwrap_or(0) + 1,
        _ => 1,
    }
}

pub(crate) fn record_boot_attempt() -> u32 {
    let path = match boot_marker_path() { Some(p) => p, None => return 1 };
    let strikes = next_strike_count(std::fs::read_to_string(&path).ok().as_deref(), VERSION);
    let _ = std::fs::write(&path, format!("{VERSION}:{strikes}"));
    strikes
}

#[cfg(test)]
mod next_strike_count_tests {
    use super::next_strike_count;

    #[test]
    fn same_version_increments_others_reset() {
        // No marker / empty → first strike.
        assert_eq!(next_strike_count(None, "7.5.7"), 1);
        assert_eq!(next_strike_count(Some(""), "7.5.7"), 1);
        // Same version increments.
        assert_eq!(next_strike_count(Some("7.5.7:1"), "7.5.7"), 2);
        assert_eq!(next_strike_count(Some("7.5.7:4"), "7.5.7"), 5);
        // A DIFFERENT version resets — a fresh binary must not inherit old strikes
        // (else an update could self-revert on its first boot).
        assert_eq!(next_strike_count(Some("7.5.6:9"), "7.5.7"), 1);
        // Garbage / malformed count never panics; resets or restarts the count.
        assert_eq!(next_strike_count(Some("garbage-no-colon"), "7.5.7"), 1);
        assert_eq!(next_strike_count(Some("7.5.7:notanumber"), "7.5.7"), 1);
        // Surrounding whitespace is trimmed before parsing.
        assert_eq!(next_strike_count(Some("  7.5.7:2\n"), "7.5.7"), 3);
    }
}
/// Clear the boot marker = "this version reached a healthy run".
pub(crate) fn mark_boot_healthy() {
    flux_webhook("healthy", "boot survived HEAL_SECS");
    if let Some(p) = boot_marker_path() { let _ = std::fs::remove_file(p); }
}
/// Arm the detached "survived HEAL_SECS → healthy" timer (decoupled from the UI loop, so a
/// normal long run clears the strike without any render-loop hook; a crash before HEAL_SECS
/// leaves the strike for the next boot to count).
pub(crate) fn arm_heal_timer() {
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(HEAL_SECS)); mark_boot_healthy(); });
}
/// At dashboard startup: record the boot attempt + arm the heal timer, then ALWAYS proceed.
///
/// LANE-Z: this used to AUTO-REVERT to the previous binary + re-exec after `CRASH_STRIKES`
/// unhealed boots. On Windows that was a foot-gun: a normal double-clicked console that exits
/// before `HEAL_SECS` (or any quick exit) accrued a "strike" every launch, so after a few launches
/// the guard silently DOWNGRADED to an older binary AND spawned a detached relaunch — the
/// "won't start / blank / runs an older version invisibly" regression. The updater now re-execs
/// ONLY after a real [U] update to a STRICTLY NEWER version; crash recovery is the explicit
/// `sigil-top revert` command. So a clean launch reaches run_tui directly: exactly ONE main()
/// entry, no auto-revert, no downgrade, no detached child. Returns false (always run).
pub(crate) fn crashloop_guard() -> bool {
    let _ = record_boot_attempt(); // kept for diagnostics (boot marker) + the manual `revert` button
    arm_heal_timer();
    false
}

/// Retained for reference / a possible opt-in future flag — the OLD auto-revert path. NOT called
/// from the launch path any more (see `crashloop_guard`). `#[allow(dead_code)]` so it documents the
/// behavior without warning.
#[allow(dead_code)]
pub(crate) fn crashloop_auto_revert() -> bool {
    let strikes = record_boot_attempt();
    if strikes < CRASH_STRIKES { arm_heal_timer(); return false; }
    let prev = match prev_binary_path() { Some(p) if p.exists() => p, _ => {
        mark_boot_healthy(); arm_heal_timer(); return false; // nothing to revert to — just run
    }};
    eprintln!("\n  {GOLD}↩ sigil-top v{VERSION} crash-looped {strikes}× — reverting to the last working binary{RESET}");
    if let Err(e) = preflight_binary(&prev) {
        eprintln!("  {RED}revert target failed pre-flight ({e}) — staying on current{RESET}");
        mark_boot_healthy(); arm_heal_timer(); return false;
    }
    if self_replace::self_replace(&prev).is_err() { mark_boot_healthy(); return false; }
    mark_boot_healthy(); // the reverted binary boots fresh under its own version counte
    let exe = match std::env::current_exe() { Ok(e) => e, Err(_) => return true };
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; let _ = std::process::Command::new(&exe).args(&args).exec(); }
    #[cfg(not(unix))]
    { let _ = std::process::Command::new(&exe).args(&args).spawn(); }
    true
}
/// `sigil-top revert` — operator's "undo a bad update" button. Pre-flights the backed-up
/// previous binary, swaps to it, and relaunches.
pub(crate) fn do_revert() {
    let prev = match prev_binary_path() {
        Some(p) if p.exists() => p,
        _ => { println!("\n  {DIM}no previous binary to revert to (no update has run yet){RESET}\n"); return; }
    };
    println!("\n  {GOLD}↩ reverting to the previous binary{RESET} — pre-flighting…");
    match preflight_binary(&prev) {
        Ok(v) => {
            if self_replace::self_replace(&prev).is_ok() {
                println!("  {GREEN}✓ reverted → v{v}{RESET}\n  {DIM}relaunching…{RESET}");
                mark_boot_healthy();
                relaunch_new_binary(&v);
            } else {
                println!("  {RED}✗ swap failed{RESET}\n");
            }
        }
        Err(e) => println!("  {RED}✗ previous binary failed pre-flight ({e}) — NOT reverting{RESET}\n"),
    }
}
