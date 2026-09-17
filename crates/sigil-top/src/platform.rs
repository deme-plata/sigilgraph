//! Windows console / process-priority FFI helpers, with Unix no-op stubs.
//!
//! Extracted verbatim from main.rs (god-file split). Self-contained: raw
//! kernel32 FFI plus local constants, no crate deps and nothing from the parent
//! module. `lower_process_priority` is Windows-only (its one call site is
//! `#[cfg(windows)]`); the other two carry Unix stubs so their call sites stay
//! unconditional.

/// Make the Windows console speak UTF-8 and process ANSI/VT escapes, so the rich
/// glyphs (◆ ● ✓ ╭─╮ ⚡ ⛓) render as real icons instead of `?`, and the colours
/// show in legacy conhost too. No-op on Unix. Raw kernel32 FFI — no extra dep.
#[cfg(windows)]
pub(crate) fn enable_rich_console() {
    type Dword = u32;
    type Handle = *mut core::ffi::c_void;
    const STD_OUTPUT_HANDLE: Dword = 0xFFFF_FFF5; // (DWORD)-11
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
    const CP_UTF8: Dword = 65001;
    extern "system" {
        fn SetConsoleOutputCP(cp: Dword) -> i32;
        fn SetConsoleCP(cp: Dword) -> i32;
        fn GetStdHandle(n: Dword) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut Dword) -> i32;
        fn SetConsoleMode(h: Handle, mode: Dword) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        SetConsoleCP(CP_UTF8);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: Dword = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
#[cfg(not(windows))]
pub(crate) fn enable_rich_console() {}

/// v0.40: on Windows, drop to BELOW_NORMAL priority class BEFORE any thread
/// spawns. The OS scheduler then always favors the user's own apps — whatever
/// sigil-top does (render, mine, opt-in sync), it can never make the desktop
/// stutter. No crate dep: two kernel32 calls.
#[cfg(windows)]
pub(crate) fn lower_process_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn SetPriorityClass(handle: isize, class: u32) -> i32;
    }
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// v3 (2026-06-19) — THE real "no TUI on Windows" root cause: Rust's `is_terminal()`
/// returns FALSE on some genuine Windows consoles (double-click / conhost / Windows
/// Terminal), so sigil-top fell through to the headless path and the dashboard never
/// opened (reproduced under Wine adverse-mode: `interactive=false` → exit before run_tui).
/// On Windows a console IS attached whenever GetConsoleWindow() is non-null — trust that
/// over is_terminal(). A genuine service/redirected run with no console returns null →
/// stays headless, so no CI/pipe regression.
#[cfg(windows)]
pub(crate) fn win_has_console() -> bool {
    extern "system" { fn GetConsoleWindow() -> *mut core::ffi::c_void; }
    unsafe { !GetConsoleWindow().is_null() }
}
#[cfg(not(windows))]
pub(crate) fn win_has_console() -> bool { false }

// ── stderr → logfile while the TUI owns the terminal (2026-09-17) ─────────────────────
//
// The unified binary runs sigil-node's own library code (braid config, hybrid checkpoint
// verdicts, coinbase rejections, …) which reports with `eprintln!`. Under ratatui's
// alternate screen every such line is painted straight over the dashboard — Viktor's
// screen on 2026-09-17 had "⚠ hybrid checkpoint at height 19628032 FAILED verification"
// spliced through the toast and stray digits in the box borders. Per-library quiet flags
// (`FLUX_DB_QUIET`, `FLUX_DB_LOG`) cannot cover a whole crate tree, so the process-level
// answer is to point file descriptor 2 at the logfile for the TUI's lifetime. Rust's
// `eprintln!` writes to fd 2 on every call (no cached handle on either platform), so the
// redirect takes effect immediately and `restore_stderr` puts the terminal back for the
// panic hook and for a normal exit.
pub(crate) struct StderrRedirect {
    #[cfg(not(windows))]
    saved: i32,
    #[cfg(windows)]
    saved: isize,
}

#[cfg(not(windows))]
pub(crate) fn redirect_stderr_to(path: &str) -> Option<StderrRedirect> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn dup(fd: i32) -> i32;
        fn dup2(src: i32, dst: i32) -> i32;
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    // SAFETY: plain POSIX fd calls on descriptors this process owns; `f` stays alive until
    // after dup2 has copied it onto fd 2, after which the original may close.
    unsafe {
        let saved = dup(2);
        if saved < 0 || dup2(f.as_raw_fd(), 2) < 0 {
            return None;
        }
        Some(StderrRedirect { saved })
    }
}

#[cfg(not(windows))]
pub(crate) fn restore_stderr(r: &StderrRedirect) {
    extern "C" {
        fn dup2(src: i32, dst: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    // SAFETY: restores the descriptor `redirect_stderr_to` saved; idempotent.
    unsafe {
        if r.saved >= 0 {
            let _ = dup2(r.saved, 2);
            let _ = close(r.saved);
        }
    }
}

#[cfg(windows)]
pub(crate) fn redirect_stderr_to(path: &str) -> Option<StderrRedirect> {
    use std::os::windows::io::IntoRawHandle;
    type Handle = isize;
    extern "system" {
        fn GetStdHandle(n: u32) -> Handle;
        fn SetStdHandle(n: u32, h: Handle) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    let h = f.into_raw_handle() as Handle; // leaked on purpose: lives as long as fd 2 points at it
    // SAFETY: Win32 std-handle table calls on handles this process owns.
    unsafe {
        let saved = GetStdHandle(STD_ERROR_HANDLE);
        if SetStdHandle(STD_ERROR_HANDLE, h) == 0 {
            return None;
        }
        Some(StderrRedirect { saved })
    }
}

#[cfg(windows)]
pub(crate) fn restore_stderr(r: &StderrRedirect) {
    extern "system" {
        fn SetStdHandle(n: u32, h: isize) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    // SAFETY: puts back the handle `redirect_stderr_to` saved; idempotent.
    unsafe {
        let _ = SetStdHandle(STD_ERROR_HANDLE, r.saved);
    }
}
