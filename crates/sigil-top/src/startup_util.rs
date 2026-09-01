//! Startup utilities extracted from main.rs (god-file split, 2026-09-01):
//! the persisted sync-mode ([F]/[Y] remember full-archive across restarts) and the
//! best-effort boot-trace breadcrumb log. Pure leaf module (std only).

/// v0.71.1 LANE-V: persisted sync mode — F/Y write it, boot reads it, so a node
/// the operator put in full-sync RESUMES full-sync after updates and restarts.
/// Fresh installs have no file -> light monitor stays the safe default.
fn sync_mode_path() -> std::path::PathBuf {
    let dir = std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .or_else(|| std::env::var("USERPROFILE").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("sync-mode")
}
pub(crate) fn persist_sync_mode(mode: &str) {
    let p = sync_mode_path();
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(&p, mode);
}
pub(crate) fn read_sync_mode() -> Option<String> {
    std::fs::read_to_string(sync_mode_path()).ok().map(|s| s.trim().to_string())
}

/// v0.64: append a startup breadcrumb to %TEMP%/sigil-top-startup.log (best-effort).
/// The Windows-startup boot trace flushes per line (each call opens+appends+closes),
/// so the LAST written line is a reliable "died here" marker under wine-verify.
pub(crate) fn boot_trace(msg: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("sigil-top-startup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", msg);
    }
}
