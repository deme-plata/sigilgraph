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

// v0.64 boot_trace moved to sigil-top-base (2026-10-07 crate split); re-exported here.
pub(crate) use sigil_top_base::boot_trace;
