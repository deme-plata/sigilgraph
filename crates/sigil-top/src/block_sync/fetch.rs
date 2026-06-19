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

// ─────────────────────────────────────────────────────────────────────────────
// LANE-A v3 — SNAPSHOT / CHECKPOINT archive format (the rsync-speed prefix path).
//
// Operator data point: rsync Win→Epsilon sustains ~100 MB/s; block sync moves
// ~1.18 MB/s (144 blk/s × 8 KB) — ~85× below the proven pipe. That gap is protocol
// serialization (per-chunk req/resp RTT + small yamux windows + per-block verify
// interleaved), NOT bandwidth. So the historical prefix is bulk-transferred as a
// verified, content-addressed SNAPSHOT — one rsync-like stream, or a few large
// flux-p2p FoldRange responses — verified stream-as-you-go, with per-block libp2p
// reserved for the live frontier only. At 100 MB/s + 200 B skeletons that's ~500k
// blk/s of transport headroom (5× the 100k goal); the ceiling then moves to
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

use sigil_header::{BlockHash, Root, SigilBlockHeaderV0};

/// Snapshot magic — "SiGil SNapshot".
pub(super) const SNAPSHOT_MAGIC: [u8; 4] = *b"SGSN";
pub(super) const SNAPSHOT_VERSION: u16 = 1;

/// One skeleton record on the snapshot wire (codec=2 'S'). 200 B fixed under bincode
/// (one u64 + 6×[u8;32], no length prefixes). Drops the ~8 KB of PQ proofs
/// (STARK/VDF/SQIsign/ProofBundle); keeps exactly what the transport-structural verify
/// + B's fold witness need.
///
/// `block_hash` (the committed BLAKE3 of the FULL header) is REQUIRED — it can't be
/// recomputed from a skeleton (the skeleton omits the proofs the hash covers): (a) the
/// linkage walk checks `rec[i].parent_hash == rec[i-1].block_hash`; (b) B's fold
/// witness is `BLAKE3-XOF(… ‖ h.hash())`, i.e. it consumes `block_hash` per block. So
/// 200 B / ~41× vs 8 KB (the earlier 168 B / 48× estimate omitted `block_hash`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct SkeletonRecord {
    pub height: u64,
    pub block_hash: BlockHash,  // committed BLAKE3 of the full header (identity + fold witness)
    pub parent_hash: BlockHash, // selected-parent (spine) link
    pub wallet_state_root: Root,
    pub dex_state_root: Root,
    pub event_log_root: Root,
    pub contract_state_root: Root,
}

impl SkeletonRecord {
    /// Producer / test side: derive a skeleton from a full header.
    pub(super) fn from_header(h: &SigilBlockHeaderV0) -> Self {
        Self {
            height: h.height,
            block_hash: h.hash(),
            parent_hash: h.parent_hash,
            wallet_state_root: h.wallet_state_root,
            dex_state_root: h.dex_state_root,
            event_log_root: h.event_log_root,
            contract_state_root: h.contract_state_root,
        }
    }
}

/// Framing prefix, sent before the record stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct SnapshotHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub base_height: u64,       // first record height (genesis for a full prefix)
    pub anchor_height: u64,     // last record height = the DNS-anchored trust point
    pub anchor_hash: BlockHash, // the trusted tip hash at anchor_height
    pub count: u64,             // records that follow
}

/// Trailer, sent after the record stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct SnapshotTrailer {
    /// BLAKE3 over the canonical bincode of every record, in order.
    pub archive_root: BlockHash,
    /// Producer SQIsign over `(archive_root ‖ anchor_height ‖ anchor_hash)`. Opaque to
    /// fetch.rs — LANE-B verifies it against the DNS-anchored producer pubkey.
    pub anchor_sig: Vec<u8>,
    /// Opaque `bincode(FoldCheckpoint)` — LANE-B's verify.rs decodes + flux_fold-verifies
    /// it (M2). Empty when the snapshot ships without the optional fold attestation.
    pub fold_blob: Vec<u8>,
}

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
    Empty,
}

/// Streaming transport-structural verifier. Feed `new(header)`, then each record in
/// order via `push`, then `finalize(trailer)`. It (1) BLAKE3-streams canonical record
/// bytes into the archive root, (2) checks height contiguity, and (3) walks the
/// parent_hash linkage. On finalize it RECOMPUTES the root and compares to the peer's
/// claim — a single bit-flip changes the root, so a malicious 100 MB/s peer cannot
/// tamper undetected. The cryptographic anchor (SQIsign over the root + the fold proof)
/// is LANE-B's job; this hands B the locally-recomputed root + the opaque sig/fold bytes.
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
        let bytes = bincode::serialize(rec).map_err(|_| SnapshotError::Encode)?;
        self.hasher.update(&bytes);
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

#[cfg(test)]
mod lane_a_snapshot_tests {
    use super::{
        SkeletonRecord, SnapshotError, SnapshotHeader, SnapshotTrailer, SnapshotVerifier,
        SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
    };

    fn rec(h: u64, bh: u8, ph: u8) -> SkeletonRecord {
        SkeletonRecord {
            height: h,
            block_hash: [bh; 32],
            parent_hash: [ph; 32],
            wallet_state_root: [0; 32],
            dex_state_root: [0; 32],
            event_log_root: [0; 32],
            contract_state_root: [0; 32],
        }
    }

    fn header(base: u64, anchor: u64, count: u64) -> SnapshotHeader {
        SnapshotHeader { magic: SNAPSHOT_MAGIC, version: SNAPSHOT_VERSION,
            base_height: base, anchor_height: anchor, anchor_hash: [anchor as u8; 32], count }
    }

    #[test]
    fn skeleton_record_is_200_bytes_on_the_wire() {
        assert_eq!(bincode::serialize(&rec(0, 1, 0)).unwrap().len(), 200);
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
}
