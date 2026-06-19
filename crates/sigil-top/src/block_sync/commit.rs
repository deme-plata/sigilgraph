//! block_sync/commit.rs — LANE-C (storage / durability)
//!
//! Owner: rocky-sync-C. Tip-cache persistence today; this is where the batched
//! write-back commit path + content-addressed BlockStore bulk-import land. Split out
//! of block_sync.rs 2026-06-19 (v3 sync sprint). Verbatim.

// ── v0.32.5: persisted tip — OFFLINE-RESILIENT COLD START ───────────────────────────────────
// The fast-snap needs a known network tip in peer_best. The eager-seed + poller fetch it from the
// CDN oracles, but if BOTH are unreachable at boot (laptop offline, CDN outage, captive portal),
// peer_best stays 0 and the monitor sits at "connecting…". Cache the last-known tip on disk each
// time it advances; on the next cold start, seed peer_best from it so the snap can STILL fire to a
// recent window. The live poller corrects it upward the instant an oracle answers. Only ever RAISES.
pub(super) fn tip_cache_path() -> std::path::PathBuf {
    let dir = std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("last-tip")
}
pub(super) fn read_persisted_tip() -> Option<u64> {
    std::fs::read_to_string(tip_cache_path()).ok()?.trim().parse::<u64>().ok().filter(|&h| h > 0)
}
pub(super) fn persist_tip(h: u64) {
    let p = tip_cache_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(p, h.to_string());
}
/// v0.36.1: drop the persisted tip so a restart doesn't re-seed a stale (pre-reset)
/// height. Called when chain-reset detection fires in the tip-poller.
pub(super) fn clear_persisted_tip() { let _ = std::fs::remove_file(tip_cache_path()); }
