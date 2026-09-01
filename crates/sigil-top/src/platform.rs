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
