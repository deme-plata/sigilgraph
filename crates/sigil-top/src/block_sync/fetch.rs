//! block_sync/fetch.rs — LANE-A (network / transport)
//!
//! Owner: rocky-sync-A. Live-tip oracle fetch + chunk alignment today; this is where
//! the windowed, multi-substream block-pack request scheduler lands. Split out of
//! block_sync.rs 2026-06-19 (v3 sync sprint). Verbatim.
use super::{HTTP_ASYNC, HTTP_BLOCKING};
use std::time::Duration;

/// v0.50 (LANE-A sync): chunk-align a base height to the nearest CHUNK boundary at/below `h`,
/// clamped to the lowest servable height `sync_base`. Used by the recent-window probe-before-snap.
pub(super) fn align_base(h: u64, chunk: u64, sync_base: u64) -> u64 {
    ((h / chunk) * chunk).max(sync_base)
}

/// Fetch the network's REAL tip height from the published `sigil-tip-live.json`.
/// v0.17.0: the monitor's `/api/v1/status` mis-routes to a near-empty sigil-rpcd that
/// returns height=2, so `set_known_tip` seeded `peer_best≈2` and the fast-snap (gated on
/// `peer_best > synced+200k`) NEVER fired → genesis crawl at ~1 blk/s. The probe is
/// clamped to frontier+CHUNK so it can't reveal the tip either. This signed-by-producer
/// JSON carries the true height (~6.7M), so it's the reliable tip source for the snap.
pub(super) async fn fetch_live_tip() -> Option<u64> {
    // 0.77: shared pooled client (keep-alive) — was a fresh Client per call.
    fetch_live_tip_inner(&HTTP_ASYNC).await
}

/// Blocking variant — runs on a DEDICATED OS thread, isolated from the busy block_sync
/// tokio runtime where the async fetch was non-deterministically starved (peer_best froze
/// → the monitor parked behind the tip). This is the reliable peer_best source.
pub(super) fn fetch_live_tip_blocking() -> Option<u64> {
    const URLS: [&str; 2] = [
        "https://sigilgraph.fluxapp.xyz/sigil-tip-live.json",
        "https://quillon.xyz/sigil-tip-live.json",
    ];
    let cb = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    // v0.29.5 (sync hardening): RACE the two CDN oracles instead of trying them sequentially.
    // The old loop hit URL[0] first and, if that CDN was slow or down, BLOCKED the full 5 s
    // timeout before even trying URL[1] — so a single bad edge node stalled cold-start tip
    // acquisition (and every poll) for 5 s. Now both fire concurrently and the FIRST positive
    // height wins, so the monitor's fast-snap fires as soon as ANY oracle answers. A dead CDN
    // costs nothing; resilience to a one-CDN outage is free.
    let (tx, rx) = std::sync::mpsc::channel::<Option<u64>>();
    for url in URLS {
        let tx = tx.clone();
        let u = format!("{url}?cb={cb}");
        std::thread::spawn(move || {
            let h = (|| -> Option<u64> {
                // 0.77: shared pooled client — was a fresh Client per racer thread per poll.
                let v = HTTP_BLOCKING.get(&u).header("cache-control", "no-cache").send().ok()?
                    .json::<serde_json::Value>().ok()?;
                v.get("height").and_then(|x| x.as_u64()).filter(|&h| h > 0)
            })();
            let _ = tx.send(h); // recv may already be gone (a faster oracle won) — harmless
        });
    }
    drop(tx); // so rx disconnects once both worker threads have answered
    // First positive answer wins the race; otherwise drain until both report (or 6 s safety).
    loop {
        match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Some(h)) => return Some(h),
            Ok(None) => continue,            // one oracle failed — keep waiting for the other
            Err(_) => return None,           // both failed / disconnected
        }
    }
}

pub(super) async fn fetch_live_tip_inner(client: &reqwest::Client) -> Option<u64> {
    const URLS: [&str; 2] = [
        "https://sigilgraph.fluxapp.xyz/sigil-tip-live.json",
        "https://quillon.xyz/sigil-tip-live.json",
    ];
    // cache-buster (per-call) so a CDN/proxy never pins the tip; the publisher uses one too.
    let cb = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    for url in URLS {
        let u = format!("{url}?cb={cb}");
        match client.get(&u).header("cache-control", "no-cache").send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    if let Some(h) = v.get("height").and_then(|x| x.as_u64()) {
                        if h > 0 { return Some(h); }
                    }
                }
                Err(e) => crate::tlog!("[tip] {url} json err: {e}"),
            },
            Err(e) => crate::tlog!("[tip] {url} get err: {e}"),
        }
    }
    None
}
