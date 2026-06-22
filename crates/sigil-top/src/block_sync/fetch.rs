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

/// Process-monotonic watermark of accepted anchor epochs (resets on restart — fine, a
/// fresh process re-reads the current anchor). Rejects replay of a superseded-but-signed
/// anchor within the freshness window.
static ANCHOR_LAST_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The REAL backend for `dns_anchor_tip()` (LANE-A). Fetch the producer-signed `v=sigil1`
/// anchor, verify it via `sigil_dns_anchor::verify_signed_anchor` (B's lane — SQIsign over
/// block_hash‖roots‖height‖epoch), gate key_id + freshness + monotonic epoch, and return
/// `(height, block_hash)`. Returns `None` on ANY failure (no source / legacy roots-only /
/// wrong key / stale / future-dated / bad sig) so `dns_anchor_tip()` stays a no-op with zero
/// regression until a real signed anchor is published. Blocking (sync) to match the
/// `dns_anchor_tip()` signature; runs off the sync poll like `fetch_live_tip_blocking`.
pub(super) fn fetch_verified_anchor_tip() -> Option<(u64, BlockHash)> {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};
    const EXPECTED_KEY_ID: &str = "d6214c8ddc0fca2b";
    const MAX_ANCHOR_AGE_SECS: u64 = 24 * 3600; // reject anchors older than this
    const FUTURE_SKEW_SECS: u64 = 300; // reject implausibly future-dated anchors

    // Trust root = the producer pubkey, PINNED by the operator (fail-safe: no pin → None →
    // no-op, NEVER a silently-fetched MITM-able key). SIGIL_ANCHOR_ALLOW_FETCH_PK=1 opts into
    // fetching it from the DNS key record (dev/convenience only — a malicious mirror can serve
    // a matching pk+tip pair). SECURITY TODO: compile the trust-root pk in once it's final.
    let producer_pk: Vec<u8> = match std::env::var("SIGIL_ANCHOR_PK_HEX")
        .ok()
        .and_then(|h| hex::decode(h.trim()).ok())
    {
        Some(pk) => pk,
        None if std::env::var("SIGIL_ANCHOR_ALLOW_FETCH_PK").as_deref() == Ok("1") => {
            fetch_producer_pk()?
        }
        None => return None, // no pinned trust root → safe no-op
    };

    // Fetch the signed `v=sigil1` anchor text (HTTP mirror of the DNS TXT; env-overridable).
    let url = std::env::var("SIGIL_ANCHOR_URL")
        .unwrap_or_else(|_| "https://sigilgraph.fluxapp.xyz/sigil-tip-anchor.txt".into());
    let txt = HTTP_BLOCKING
        .get(&url)
        .header("cache-control", "no-cache")
        .send()
        .ok()?
        .text()
        .ok()?;

    let a = sigil_dns_anchor::decode(&txt).ok()?;
    // Reject legacy roots-only records up front (no block_hash/epoch → can't anchor a fold).
    let epoch = a.epoch?;
    let bh_hex = a.block_hash_hex.as_deref()?;

    // key_id must be the expected producer key (defense vs a different signer's record).
    if a.key_id.as_str() != EXPECTED_KEY_ID {
        return None;
    }
    // Monotonic replay guard (cheap; before the expensive verify). Only committed AFTER verify.
    if epoch <= ANCHOR_LAST_EPOCH.load(Ordering::Relaxed) {
        return None;
    }
    // Freshness: reject stale OR implausibly future-dated. The sig binds epoch, so a future
    // date implies a buggy/compromised producer, not a peer — reject defensively.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(epoch) > MAX_ANCHOR_AGE_SECS || epoch > now + FUTURE_SKEW_SECS {
        return None;
    }

    // Cryptographic verify (SQIsign, sigil-dns-anchor — keeps sigil-top flux-sqisign-free).
    if !sigil_dns_anchor::verify_signed_anchor(&a, &producer_pk).unwrap_or(false) {
        return None;
    }

    // Decode the now-verified block_hash → [u8; 32].
    let bh: BlockHash = hex::decode(bh_hex).ok()?.try_into().ok()?;
    // Authoritative monotonic commit: atomic, race-free, and poison-free (only verified
    // records reach here). If we lost a race to a higher epoch, reject this one.
    let prev = ANCHOR_LAST_EPOCH.fetch_max(epoch, Ordering::AcqRel);
    if epoch <= prev {
        return None;
    }
    Some((a.height, bh))
}

/// Fetch the DNS-published producer pubkey (`sigil-anchor-key.json` → `producer_pk_hex`).
fn fetch_producer_pk() -> Option<Vec<u8>> {
    let url = std::env::var("SIGIL_ANCHOR_KEY_URL")
        .unwrap_or_else(|_| "https://sigilgraph.fluxapp.xyz/sigil-anchor-key.json".into());
    let v = HTTP_BLOCKING
        .get(&url)
        .header("cache-control", "no-cache")
        .send()
        .ok()?
        .json::<serde_json::Value>()
        .ok()?;
    hex::decode(v.get("producer_pk_hex")?.as_str()?.trim()).ok()
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

// ─────────────────────────────────────────────────────────────────────────────
// LANE-A v3 — SNAPSHOT / CHECKPOINT archive format (the rsync-speed prefix path).
//
// Operator data point: rsync Win→Epsilon sustains ~100 MB/s; block sync moves
// ~1.18 MB/s (144 blk/s × 8 KB) — ~85× below the proven pipe. That gap is protocol
// serialization (per-chunk req/resp RTT + small yamux windows + per-block verify
// interleaved), NOT bandwidth. So the historical prefix is bulk-transferred as a
// verified, content-addressed SNAPSHOT — one rsync-like stream, or a few large
// flux-p2p FoldRange responses — verified stream-as-you-go, with per-block libp2p
// reserved for the live frontier only. At 100 MB/s + 72 B skeletons that's ~1.4M
// blk/s of transport headroom (14× the 100k goal); the ceiling then moves to
// verify+commit (LANE-B/C).
//
// LANE SPLIT: fetch.rs (here) owns the FORMAT + the TRANSPORT-STRUCTURAL verify
// (parse, BLAKE3-stream archive root, parent_hash linkage walk, height contiguity).
// The CRYPTOGRAPHIC verify — the producer's SQIsign over the root + the flux_fold
// range proof — is LANE-B's verify.rs (needs the producer pubkey + the flux-fold
// dep, both M2-gated). fetch.rs carries those as OPAQUE bytes and hands them to B,
// so this compiles TODAY on sigil-top's existing deps (blake3 + bincode + serde +
// sigil-header). Dead-code until lead wires the snapshot path + rules on the
// elision/anchor trust bargain. Spec: docs/SIGIL_SKELETON_CODEC2_v0.md.
// ─────────────────────────────────────────────────────────────────────────────

// Wire types (SkeletonRecord / SnapshotHeader / SnapshotTrailer / SNAPSHOT_MAGIC /
// SNAPSHOT_VERSION) live in sigil-header so the sigil-node SERVER shares ONE definition
// with this CLIENT. Client-only logic (SnapshotVerifier / SnapshotVerified / SnapshotError
// / pull_snapshot) stays below.
use sigil_header::{
    BlockHash, SkeletonRecord, SnapshotHeader, SnapshotTrailer, SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};

/// What the transport-structural verify proves; handed to LANE-B for the crypto finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SnapshotVerified {
    pub base_height: u64,
    pub anchor_height: u64,
    pub anchor_hash: BlockHash,
    pub archive_root: BlockHash, // RECOMPUTED locally, never the peer's claim
    pub anchor_sig: Vec<u8>,     // → B verifies vs the DNS producer pubkey
    pub fold_blob: Vec<u8>,      // → B flux_fold-verifies (M2)
    pub records: usize,
}

/// Fail-loud rejection reasons (the peer is benched on any of these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SnapshotError {
    BadMagic,
    BadVersion(u16),
    CountMismatch { expected: u64, got: usize },
    NonContiguousHeight { at: u64, expected: u64 },
    LinkageBreak { at: u64 }, // parent_hash != prev.block_hash
    RootMismatch,             // recomputed archive_root != trailer claim
    Encode,                   // canonical re-encode failed (unreachable for fixed struct)
    BadHeader,                // 'P' header describes an inconsistent / oversized prefix
    Empty,
}

/// Streaming transport-structural verifier. Feed `new(header)`, then each record in
/// order via `push`, then `finalize(trailer)`. It (1) BLAKE3-streams canonical record
/// bytes into the archive root, (2) checks height contiguity, and (3) walks the
/// parent_hash linkage. On finalize it RECOMPUTES the root and compares to the peer's
/// claim — a single bit-flip changes the root, so a malicious 100 MB/s peer cannot
/// tamper undetected. The cryptographic anchor (SQIsign over the root + the fold proof)
/// is LANE-B's job; this hands B the locally-recomputed root + the opaque sig/fold bytes.
#[derive(Debug)]
pub(super) struct SnapshotVerifier {
    hasher: blake3::Hasher,
    base_height: u64,
    anchor_height: u64,
    anchor_hash: BlockHash,
    expected_count: u64,
    seen: u64,
    prev_block_hash: Option<BlockHash>,
}

impl SnapshotVerifier {
    pub(super) fn new(header: &SnapshotHeader) -> Result<Self, SnapshotError> {
        if header.magic != SNAPSHOT_MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        if header.version != SNAPSHOT_VERSION {
            return Err(SnapshotError::BadVersion(header.version));
        }
        Ok(Self {
            hasher: blake3::Hasher::new(),
            base_height: header.base_height,
            anchor_height: header.anchor_height,
            anchor_hash: header.anchor_hash,
            expected_count: header.count,
            seen: 0,
            prev_block_hash: None,
        })
    }

    /// Feed the next record (must arrive in height order). O(1): folds canonical bytes
    /// into the running BLAKE3 root and checks contiguity + spine linkage.
    pub(super) fn push(&mut self, rec: &SkeletonRecord) -> Result<(), SnapshotError> {
        let expected_h = self.base_height + self.seen;
        if rec.height != expected_h {
            return Err(SnapshotError::NonContiguousHeight { at: rec.height, expected: expected_h });
        }
        if let Some(prev) = self.prev_block_hash {
            if rec.parent_hash != prev {
                return Err(SnapshotError::LinkageBreak { at: rec.height });
            }
        }
        // Hash the canonical 72 B layout field-by-field — BYTE-IDENTICAL to bincode(rec)
        // (bincode default = LE fixint: u64 LE ‖ [u8;32] ‖ [u8;32] = 72 B, pinned by
        // skeleton_record_is_72_bytes_on_the_wire) but with NO per-record bincode alloc/encode.
        // This is the verify half of the lead's #539 "stop re-encoding" fix: it drops 1 of the
        // 3 bincode round-trips/record (deserialize + verify-encode + commit-encode); C's
        // append-raw drops another → toward closing the 39,112 → 92.6k blk/s gap.
        // archive_root is unchanged: the producer hashes the same 72 B per record.
        self.hasher.update(&rec.height.to_le_bytes());
        self.hasher.update(&rec.block_hash);
        self.hasher.update(&rec.parent_hash);
        self.prev_block_hash = Some(rec.block_hash);
        self.seen += 1;
        Ok(())
    }

    /// Finalize against the trailer: check count + recompute the root, return the proven
    /// facts for LANE-B's cryptographic finish.
    pub(super) fn finalize(self, trailer: &SnapshotTrailer) -> Result<SnapshotVerified, SnapshotError> {
        if self.seen == 0 {
            return Err(SnapshotError::Empty);
        }
        if self.seen != self.expected_count {
            return Err(SnapshotError::CountMismatch { expected: self.expected_count, got: self.seen as usize });
        }
        let root: BlockHash = *self.hasher.finalize().as_bytes();
        if root != trailer.archive_root {
            return Err(SnapshotError::RootMismatch);
        }
        Ok(SnapshotVerified {
            base_height: self.base_height,
            anchor_height: self.anchor_height,
            anchor_hash: self.anchor_hash,
            archive_root: root,
            anchor_sig: trailer.anchor_sig.clone(),
            fold_blob: trailer.fold_blob.clone(),
            records: self.seen as usize,
        })
    }
}

/// Pull + structurally-verify a snapshot of the historical prefix from `peers` over the
/// flux-p2p request-response channel. `send` wraps `net.send_request` so fetch.rs needn't
/// name `libp2p::PeerId` (sigil-top has no direct libp2p dep); launch() passes
/// `|peer, body| net.send_request(peer, body)`. `commit(&page_records)` is
/// launch()'s `store.put_block_raw` seam — called per VERIFIED record so the skeletons
/// land WITHOUT ever holding them all (the OOM latch: one page resident, dropped after
/// commit). Returns the crypto-facts for LANE-B's fast_forward_to_anchored_checkpoint;
/// `Err` → caller benches the peer + falls back to codec=1 (zero regression).
///
/// codec assignment (reuses the existing `BackfillReq.codec: u8` — NO struct change in
/// mod.rs): `2` = skeleton page → `'S'`; `3` = snapshot header → `'P'`; `4` = fold+trailer
/// → `'F'`. An old responder hits its default arm on codec ≥ 2 → serves `'H'`/`'Z'`/empty
/// → not `'P'` → we treat it as "no snapshot here" and the caller downgrades. Graceful.
///
/// `'F'` body = `bincode(SnapshotTrailer)` with the `FoldCheckpoint` carried OPAQUE inside
/// `trailer.fold_blob` — so this fn needs no flux-fold dep; LANE-B decodes the blob.
pub(super) async fn pull_snapshot<P, F, Fut>(
    peers: &[P],
    send: F,
    mut commit: impl FnMut(&[SkeletonRecord]),
) -> Result<SnapshotVerified, SnapshotError>
where
    P: Clone,
    F: Fn(P, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    const PAGE: u64 = 50_000; // ~3.6 MB/page at 72 B (SIGIL_SNAP_PAGE)
    const MAX_PAGE_RECS: usize = 100_000; // hard per-page guard (resource attack)
    const MAX_SNAPSHOT_RECORDS: u64 = 50_000_000; // sane prefix bound (chain ~6.7M today)
    const MAX_P_BYTES: usize = 4096; // 'P' is ~90 B fixed — cap a forged length prefix
    const MAX_S_BYTES: usize = MAX_PAGE_RECS * 80 + 64; // ~8 MB — bounds Vec<SkeletonRecord> alloc
    const MAX_F_BYTES: usize = 64 * 1024 * 1024; // trailer cap (M1 tiny; M2 fold_blob — B-chunked)
    if peers.is_empty() {
        return Err(SnapshotError::Empty);
    }
    let mk = |from: u64, to: u64, codec: u8| -> Result<Vec<u8>, SnapshotError> {
        serde_json::to_vec(&super::BackfillReq { from, to, headers_only: true, codec })
            .map_err(|_| SnapshotError::Encode)
    };

    // (a) DISCOVER + framing header (codec=3 → 'P'); the first peer that answers 'P' serves.
    let mut server: Option<P> = None;
    let mut header: Option<SnapshotHeader> = None;
    for p in peers {
        let body = mk(0, 0, 3)?;
        if let Ok(resp) = send(p.clone(), body).await {
            if resp.first() == Some(&b'P') && resp.len() <= MAX_P_BYTES {
                if let Ok(h) = bincode::deserialize::<SnapshotHeader>(&resp[1..]) {
                    server = Some(p.clone());
                    header = Some(h);
                    break;
                }
            }
        }
    }
    let header = header.ok_or(SnapshotError::Empty)?; // no 'P' anywhere → caller downgrades
    let server = server.ok_or(SnapshotError::Empty)?;
    // The header must describe a sane CONTIGUOUS prefix — a lying `count` would otherwise
    // loop the stream, and an inconsistent range lets a peer steer us off the real prefix.
    if header.count == 0
        || header.count > MAX_SNAPSHOT_RECORDS
        || header.anchor_height < header.base_height
        || header.count != header.anchor_height - header.base_height + 1
    {
        return Err(SnapshotError::BadHeader);
    }
    let mut verifier = SnapshotVerifier::new(&header)?; // BadMagic / BadVersion fail here

    // (b) STREAM skeleton pages (codec=2 → 'S'), commit-as-you-stream, DROP each page.
    let base = header.base_height;
    let total = header.count;
    let mut done: u64 = 0;
    while done < total {
        let from = base + done;
        let to = (from + PAGE).min(base + total);
        let resp = send(server.clone(), mk(from, to, 2)?)
            .await
            .map_err(|_| SnapshotError::Empty)?;
        if resp.first() != Some(&b'S') || resp.len() > MAX_S_BYTES {
            return Err(SnapshotError::Empty);
        }
        let page: Vec<SkeletonRecord> =
            bincode::deserialize(&resp[1..]).map_err(|_| SnapshotError::Encode)?;
        if page.is_empty() || page.len() > MAX_PAGE_RECS {
            return Err(SnapshotError::Encode);
        }
        for rec in &page {
            verifier.push(rec)?; // contiguity + linkage + running BLAKE3 root
        }
        commit(&page); // per-page flat SkeletonStore append (10M-blk/s path)
        done += page.len() as u64;
        // `page` dropped here → only one page ever resident (the OOM latch).
    }

    // (c) FOLD + TRAILER (codec=4 → 'F'); finalize RECOMPUTES the root vs the trailer claim.
    let resp = send(server, mk(base, header.anchor_height, 4)?)
        .await
        .map_err(|_| SnapshotError::Empty)?;
    if resp.first() != Some(&b'F') || resp.len() > MAX_F_BYTES {
        return Err(SnapshotError::Empty);
    }
    let trailer: SnapshotTrailer =
        bincode::deserialize(&resp[1..]).map_err(|_| SnapshotError::Encode)?;
    verifier.finalize(&trailer)
}

#[cfg(test)]
mod lane_a_snapshot_tests {
    use super::{
        SkeletonRecord, SnapshotError, SnapshotHeader, SnapshotTrailer, SnapshotVerifier,
        SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
    };

    fn rec(h: u64, bh: u8, ph: u8) -> SkeletonRecord {
        SkeletonRecord { height: h, block_hash: [bh; 32], parent_hash: [ph; 32] }
    }

    fn header(base: u64, anchor: u64, count: u64) -> SnapshotHeader {
        SnapshotHeader { magic: SNAPSHOT_MAGIC, version: SNAPSHOT_VERSION,
            base_height: base, anchor_height: anchor, anchor_hash: [anchor as u8; 32], count }
    }

    #[test]
    fn skeleton_record_is_72_bytes_on_the_wire() {
        assert_eq!(bincode::serialize(&rec(0, 1, 0)).unwrap().len(), 72);
    }

    #[test]
    fn valid_snapshot_verifies_and_recomputes_its_own_root() {
        let recs = [rec(0, 10, 0), rec(1, 11, 10), rec(2, 12, 11)]; // linked spine
        let mut h = blake3::Hasher::new();
        for r in &recs { h.update(&bincode::serialize(r).unwrap()); }
        let root = *h.finalize().as_bytes();
        let mut v = SnapshotVerifier::new(&header(0, 2, 3)).unwrap();
        for r in &recs { v.push(r).unwrap(); }
        let ok = v.finalize(&SnapshotTrailer { archive_root: root, anchor_sig: vec![], fold_blob: vec![] }).unwrap();
        assert_eq!(ok.archive_root, root);
        assert_eq!(ok.records, 3);
        assert_eq!(ok.anchor_height, 2);
    }

    // RUNTIME end-to-end: drive the WHOLE client path — pull_snapshot's discover/stream/finalize
    // loop + SnapshotVerifier + the commit-hook into the flat SkeletonStore — against a synthetic
    // in-process codec=2 server (mock `send` answering 'P'/'S'/'F' by request codec). Proves the
    // path RUNS (not just compiles) and measures end-to-end blk/s. SIGIL_SNAP_BENCH=N for a big run.
    #[tokio::test]
    async fn snapshot_pull_end_to_end_into_store() {
        use std::sync::Arc;
        let n: u64 = std::env::var("SIGIL_SNAP_BENCH").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
        // Linked spine: parent_hash[i] == block_hash[i-1] (u8 fill is self-consistent under wrap).
        let recs: Arc<Vec<SkeletonRecord>> =
            Arc::new((0..n).map(|i| rec(i, i as u8, i.wrapping_sub(1) as u8)).collect());
        // archive_root = BLAKE3 over bincode(each record) — exactly what the verifier recomputes.
        let mut hh = blake3::Hasher::new();
        for r in recs.iter() { hh.update(&bincode::serialize(r).unwrap()); }
        let root = *hh.finalize().as_bytes();
        let hdr_b = Arc::new({ let mut b = vec![b'P']; b.extend_from_slice(&bincode::serialize(&header(0, n - 1, n)).unwrap()); b });
        let tr_b = Arc::new({ let mut b = vec![b'F']; b.extend_from_slice(&bincode::serialize(
            &SnapshotTrailer { archive_root: root, anchor_sig: vec![], fold_blob: vec![] }).unwrap()); b });
        // Pre-serialize every 'S' page ONCE — a real server serves pre-encoded pages from disk;
        // encoding per-request would charge the CLIENT bench for the mock's encode time.
        const PAGE: u64 = 50_000;
        let pages = {
            let mut m: std::collections::HashMap<u64, Arc<Vec<u8>>> = std::collections::HashMap::new();
            let mut from = 0u64;
            while from < n {
                let to = (from + PAGE).min(n);
                let mut b = vec![b'S'];
                b.extend_from_slice(&bincode::serialize(&recs[from as usize..to as usize].to_vec()).unwrap());
                m.insert(from, Arc::new(b));
                from += PAGE;
            }
            Arc::new(m)
        };
        // Synthetic codec=2 server: returns the pre-built 'P'/'S'/'F' bytes by request codec/from.
        let send = {
            let (pages, hdr_b, tr_b) = (pages.clone(), hdr_b.clone(), tr_b.clone());
            move |_peer: (), req: Vec<u8>| {
                let (pages, hdr_b, tr_b) = (pages.clone(), hdr_b.clone(), tr_b.clone());
                async move {
                    let j: serde_json::Value = serde_json::from_slice(&req).map_err(|e| e.to_string())?;
                    match j["codec"].as_u64().unwrap_or(0) {
                        3 => Ok::<Vec<u8>, String>((*hdr_b).clone()),
                        2 => {
                            let from = j["from"].as_u64().unwrap_or(0);
                            pages.get(&from).map(|p| (**p).clone()).ok_or_else(|| format!("no page at {from}"))
                        }
                        4 => Ok((*tr_b).clone()),
                        _ => Err("bad codec".to_string()),
                    }
                }
            }
        };
        let tmp = std::env::temp_dir().join(format!("snap-e2e-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut skel = crate::block_sync::SkeletonStore::open(&tmp, 0).unwrap();
        let peers = vec![()];
        let t0 = std::time::Instant::now();
        let v = super::pull_snapshot(&peers, send, |page| { let _ = skel.append(page); })
            .await
            .expect("pull_snapshot end-to-end");
        let dt = t0.elapsed().as_secs_f64().max(1e-9);
        assert_eq!(v.records, n as usize, "all records pulled");
        assert_eq!(v.anchor_height, n - 1);
        assert_eq!(v.archive_root, root, "client recomputed root == source root");
        assert_eq!(skel.count(), n, "all records landed in the flat SkeletonStore");
        eprintln!("[snap-pull-e2e] {} records pull→verify→flat-append in {:.4}s = {:.0} blk/s (full client path, in-proc codec=2)",
            n, dt, n as f64 / dt);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn linkage_break_is_rejected() {
        let mut v = SnapshotVerifier::new(&header(0, 1, 2)).unwrap();
        v.push(&rec(0, 10, 0)).unwrap();
        // h1 claims parent=99 but prev block_hash was 10 → break
        assert_eq!(v.push(&rec(1, 11, 99)).unwrap_err(), SnapshotError::LinkageBreak { at: 1 });
    }

    #[test]
    fn noncontiguous_height_is_rejected() {
        let mut v = SnapshotVerifier::new(&header(0, 5, 2)).unwrap();
        v.push(&rec(0, 10, 0)).unwrap();
        assert_eq!(v.push(&rec(2, 12, 10)).unwrap_err(),
            SnapshotError::NonContiguousHeight { at: 2, expected: 1 });
    }

    #[test]
    fn tampered_root_is_rejected() {
        let mut v = SnapshotVerifier::new(&header(0, 0, 1)).unwrap();
        v.push(&rec(0, 10, 0)).unwrap();
        assert_eq!(
            v.finalize(&SnapshotTrailer { archive_root: [0xff; 32], anchor_sig: vec![], fold_blob: vec![] }).unwrap_err(),
            SnapshotError::RootMismatch
        );
    }

    #[test]
    fn count_mismatch_is_rejected() {
        let mut v = SnapshotVerifier::new(&header(0, 2, 3)).unwrap(); // claims 3
        v.push(&rec(0, 10, 0)).unwrap();
        let err = v.finalize(&SnapshotTrailer { archive_root: [0; 32], anchor_sig: vec![], fold_blob: vec![] }).unwrap_err();
        assert_eq!(err, SnapshotError::CountMismatch { expected: 3, got: 1 });
    }

    #[test]
    fn bad_magic_rejected() {
        let h = SnapshotHeader { magic: *b"XXXX", version: SNAPSHOT_VERSION,
            base_height: 0, anchor_height: 0, anchor_hash: [0; 32], count: 1 };
        assert_eq!(SnapshotVerifier::new(&h).unwrap_err(), SnapshotError::BadMagic);
    }

    /// LANE-A micro-bench: the decode+verify half of snapshot sync (BLAKE3 archive-root
    /// stream + 32 B parent-linkage walk + contiguity), isolated from network and store.
    /// `#[ignore]` so it never slows the normal suite -- run explicitly for the number:
    ///   <test-bin> bench_snapshot_verifier_throughput --ignored --nocapture
    /// This bounds the LANE-A contribution; the full decode+verify+COMMIT number (the
    /// LANE-B/C cost that gates 92.6k) is the lead's store-backed harness.
    #[test]
    #[ignore]
    fn bench_snapshot_verifier_throughput() {
        const N: u64 = 100_000;
        let mut recs: Vec<SkeletonRecord> = Vec::with_capacity(N as usize);
        let mut prev = [0u8; 32];
        for i in 0..N {
            let bh = *blake3::hash(&i.to_le_bytes()).as_bytes();
            recs.push(SkeletonRecord { height: i, block_hash: bh, parent_hash: prev });
            prev = bh;
        }
        // Producer-side archive_root (what the verifier must reproduce).
        let mut hh = blake3::Hasher::new();
        for r in &recs {
            hh.update(&bincode::serialize(r).unwrap());
        }
        let root = *hh.finalize().as_bytes();
        let hdr = SnapshotHeader {
            magic: SNAPSHOT_MAGIC, version: SNAPSHOT_VERSION,
            base_height: 0, anchor_height: N - 1, anchor_hash: prev, count: N,
        };
        let trailer = SnapshotTrailer { archive_root: root, anchor_sig: vec![], fold_blob: vec![] };

        let t = std::time::Instant::now();
        let mut v = SnapshotVerifier::new(&hdr).unwrap();
        for r in &recs {
            v.push(r).unwrap();
        }
        let out = v.finalize(&trailer).unwrap();
        let dt = t.elapsed();

        let blk_s = N as f64 / dt.as_secs_f64();
        eprintln!(
            "[bench] SnapshotVerifier: {N} recs ({} MB wire) verified in {:?} = {:.0} blk/s",
            (N * 72) / 1_000_000,
            dt,
            blk_s,
        );
        assert_eq!(out.records, N as usize);
        assert_eq!(out.archive_root, root);
    }

    /// Proves the flat append-only skeleton-store ceiling that breaks the commit wall
    /// (lead #491: flux-db KV bulk commit caps at ~3,863 blk/s; the prefix must NOT go
    /// through KV). 72 B sequential records + ONE durable fsync (the 2-phase watermark cost).
    /// `#[ignore]` — run for the number:
    ///   <test-bin> bench_flat_append_skeleton_store --ignored --nocapture
    /// In-tree + reproducible on the capped build, vs the standalone measurement (2.2M-10M).
    /// This is the LANE-C store's target ceiling; LANE-A feeds it the raw 72 B records.
    #[test]
    #[ignore]
    fn bench_flat_append_skeleton_store() {
        use std::io::Write;
        const N: u64 = 1_000_000;
        let rec = [0u8; 72]; // a skeleton stride
        let path = std::env::temp_dir().join("sigil_flat_skel_bench.dat");

        let t = std::time::Instant::now();
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut w = std::io::BufWriter::with_capacity(4 << 20, f);
            for _ in 0..N {
                w.write_all(&rec).unwrap();
            }
            w.into_inner().unwrap().sync_all().unwrap(); // ONE fsync = the durable watermark
        }
        let dt = t.elapsed();
        let blk_s = N as f64 / dt.as_secs_f64();
        eprintln!(
            "[bench] flat-append skeleton store: {N} recs ({} MB) durable in {:?} = {:.0} blk/s (vs flux-db KV 3,863)",
            (N * 72) / 1_000_000,
            dt,
            blk_s,
        );
        let _ = std::fs::remove_file(&path);
        assert!(blk_s > 1_000_000.0, "flat append must clear 1M blk/s, got {blk_s:.0}");
    }
}
