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

// ─────────────────────────────────────────────────────────────────────────────
// LANE-A v3 sync sprint (rocky-sync-A) — windowed / multi-substream / adaptive
// block-pack SCHEDULER policy.
//
// WHY this is pure policy and not the wire call itself: the actual fetch is
// `net.send_request(peer, payload)` where `peer: libp2p::PeerId`. sigil-top does
// NOT depend on `libp2p` directly (only transitively via flux-p2p), and flux-p2p
// does not re-export PeerId — so the type is only *nameable* inside launch(), where
// it is inferred from `net.connected_peers()`. The wire spawn therefore stays in
// mod.rs; LANE-A owns the POLICY that drives it: WHICH ranges to put in flight, HOW
// MANY, HOW BIG, and HOW WIDELY to hedge a lead range across peers. All pure fns →
// unit-tested OFFLINE, independent of Delta (the only SIGIL build box, down until
// ~July — so no flux_combo here; lead compile-verifies on the capped Epsilon build).
//
// launch() (lead, mod.rs) wires these in 3 spots, replacing the inline heuristics:
//   • the `max_inflight` turbo boost (≈ mod.rs:523) → `adaptive_inflight(..)`
//   • the per-request `use_chunk` size              → `adaptive_chunk(..)`
//   • the frontier-probe fan width (`eff_fan`)       → `hedge_fanout(..)`
// ─────────────────────────────────────────────────────────────────────────────

/// AIMD pack-size control (charter item 2: "grow pack size until WG MTU/CPU is the
/// limiter; measure, don't guess"). Multiplicative-increase the chunk while the last
/// reply came back FULL (the peer had at least a whole chunk more to give) AND inside
/// the RTT budget; halve it the moment a reply is slow or empty (timeout / behind
/// peer / WG congestion). Pure — callers pass the measured RTT + whether it filled.
///
/// * `cur`           – current chunk (headers / request)
/// * `last_rtt_ms`   – wall-clock of the last completed request
/// * `last_was_full` – got == requested (peer had ≥ a full chunk to serve)
/// * `target_rtt_ms` – RTT budget; above 2× it we're pipelining too coarsely
/// * `min_c`/`max_c` – clamp (responder caps a reply at 4096 items ⇒ `max_c ≤ 4096`)
pub(super) fn adaptive_chunk(
    cur: u64,
    last_rtt_ms: u64,
    last_was_full: bool,
    target_rtt_ms: u64,
    min_c: u64,
    max_c: u64,
) -> u64 {
    let next = if last_was_full && last_rtt_ms <= target_rtt_ms {
        cur.saturating_mul(2) // headroom → multiplicative increase, until RTT/cap bites
    } else if !last_was_full || last_rtt_ms > target_rtt_ms.saturating_mul(2) {
        (cur / 2).max(min_c) // empty / slow → multiplicative decrease, back off hard
    } else {
        cur // in-band → hold
    };
    next.clamp(min_c, max_c)
}

/// How many DISTINCT peers to send the SAME lead range to (charter item 3: "so one
/// slow peer/stream doesn't stall the window"). Request-hedging: over a lossy WG link
/// a single in-flight stream can sit behind one dropped packet for a whole RTO; issuing
/// the lead range to a few peers and taking the first non-empty reply hides that tail.
/// Cold start (nothing landed yet) or a stalled frontier → widen; warm + flowing → 1
/// (don't burn peer serve capacity once the pipe is full). Clamped to peers available.
pub(super) fn hedge_fanout(
    cold_start: bool,
    n_healthy: usize,
    frontier_stalled_secs: u64,
    base: usize,
) -> usize {
    if n_healthy == 0 {
        return 0;
    }
    let want = if cold_start {
        n_healthy // hedge across EVERYONE to land the very first block fast
    } else if frontier_stalled_secs >= 6 {
        base.max(3) // unstick a wedged frontier with a wider hedge
    } else {
        1 // flowing → single best peer, leave the rest serving other followers
    };
    // SAFE: the `n_healthy == 0` guard above guarantees `n_healthy ≥ 1` here, so the
    // clamp bounds `(1, n_healthy)` are always valid (`1 ≤ n_healthy`) — no panic.
    want.clamp(1, n_healthy)
}

/// Adaptive in-flight window depth (charter item 1). Pure form of the inline turbo
/// boost, refined with an RTT congestion signal: scale up with continuous-BW momentum
/// (continuity `score` + PID `pid_rate`) but back OFF as RTT inflates — a climbing RTT
/// means the WG pipe / responder is saturating, and piling on more in-flight packs past
/// that point only deepens the queue (bufferbloat) without adding goodput. Honors the
/// latch lesson: the `hi` ceiling bounds buffered chunks so the follower never OOMs.
///
/// * `base`     – operator floor (`SIGIL_SYNC_INFLIGHT`)
/// * `score`    – `BandwidthContinuity` score, 0..1
/// * `pid_rate` – continuity PID rate estimate (blk/s-ish)
/// * `rtt_ms`   – recent mean request RTT
/// * `hi`       – hard ceiling (buffer / OOM bound)
pub(super) fn adaptive_inflight(
    base: usize,
    score: f64,
    pid_rate: f64,
    rtt_ms: u64,
    hi: usize,
) -> usize {
    let rate_boost = (pid_rate / 50.0).clamp(0.5, 2.0);
    // congestion taper: full boost at/under 250 ms, degrading toward 0.5 as RTT → 2 s.
    let rtt_taper = if rtt_ms <= 250 {
        1.0
    } else {
        (1.0 - (rtt_ms.saturating_sub(250) as f64) / 1750.0).clamp(0.5, 1.0)
    };
    let scaled = (base as f64) * (0.5 + score * 1.5) * rate_boost * rtt_taper;
    (scaled.max(2.0) as usize).clamp(2, hi)
}

#[cfg(test)]
mod lane_a_sched_tests {
    use super::{adaptive_chunk, adaptive_inflight, hedge_fanout};

    #[test]
    fn chunk_grows_when_full_and_fast_then_clamps_at_cap() {
        assert_eq!(adaptive_chunk(512, 100, true, 800, 256, 4096), 1024); // full+fast → double
        assert_eq!(adaptive_chunk(4096, 100, true, 800, 256, 4096), 4096); // clamped at responder cap
    }

    #[test]
    fn chunk_backs_off_on_slow_or_empty() {
        assert_eq!(adaptive_chunk(2048, 100, false, 800, 256, 4096), 1024); // empty → halve
        assert_eq!(adaptive_chunk(2048, 2000, true, 800, 256, 4096), 1024); // RTT > 2× budget → halve
        assert_eq!(adaptive_chunk(256, 100, false, 800, 256, 4096), 256); // never below min
    }

    #[test]
    fn hedge_wide_when_cold_or_stalled_else_single() {
        assert_eq!(hedge_fanout(true, 5, 0, 2), 5); // cold → hedge everyone
        assert_eq!(hedge_fanout(false, 5, 8, 2), 3); // stalled 8 s → widen to ≥3
        assert_eq!(hedge_fanout(false, 5, 0, 2), 1); // flowing → single peer
        assert_eq!(hedge_fanout(false, 0, 0, 2), 0); // no peers → 0
        assert_eq!(hedge_fanout(true, 2, 0, 9), 2); // clamp to peers available
    }

    #[test]
    fn inflight_scales_with_momentum_but_tapers_on_rtt() {
        let hot = adaptive_inflight(4, 0.5, 50.0, 100, 16); // RTT inside budget
        let bloated = adaptive_inflight(4, 0.5, 50.0, 2000, 16); // RTT saturating
        assert!(hot > bloated, "RTT congestion must taper the window ({hot} !> {bloated})");
        assert!((2..=16).contains(&hot) && (2..=16).contains(&bloated));
        assert_eq!(adaptive_inflight(8, 0.0, 1.0, 50, 16), 2); // floor never drops below 2
    }
}
