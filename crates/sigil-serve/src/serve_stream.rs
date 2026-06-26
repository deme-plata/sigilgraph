//! Windowed, ACK-paced, multi-substream snapshot-page streaming for the producer.
//!
//! Why this exists: at the v7 target (≥100k blk/s) the wall is no longer gossip
//! delivery (chronos: 99.97% @2% loss) but the producer pushing pages into a
//! single WireGuard tunnel — where a lost segment head-of-line-blocks everything
//! behind it and a single fat window bufferbloats the kernel queue. DeepSeek's
//! prescription: **independent per-substream credit windows** (not one big
//! window) over **N striped substreams** (redundancy/fan-out = 2 is the measured
//! sweet spot: 99.97% delivery @2% loss vs 98% at 1; don't over-replicate), with
//! **split + retransmit** rather than duplication (duplication burns ~48%
//! bandwidth for the last 2% of loss; striping + targeted retransmit reaches
//! >99.97% at <2% overhead). The client dedups by page id via a [`PageBitmap`].
//!
//! This module is the *policy* — what page goes on which substream, with what
//! pacing and retransmit. The *transport* (the actual WG substreams) is
//! [`ChunkSink`], implemented by rocky-sync-A's `sigil-net`. The global
//! cross-lane rate ceiling is [`RateGate`], implemented by viktor-v7-coord's
//! `sigil-synctune` token-bucket spine. Both are traits so this crate builds and
//! tests standalone with the provided defaults.

use sigil_synctune::{Clock, RateGate, Stage};
use std::collections::VecDeque;

/// A unit of work: one skeleton page, identified by its base height (unique and
/// monotonic, so it doubles as the client's dedup key — no extra wire field).
pub type PageId = u64;

/// One scheduled send: page `page_id` on substream `sub`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Send {
    pub sub: usize,
    pub page_id: PageId,
}

/// Tuning for [`ServeStreamPlanner`]. Defaults follow DeepSeek's 10Gbit/3.6MB-page
/// guidance; v7-autotune sweeps these in chronos virtual-time.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Number of parallel substreams = the fan-out/"redundancy" degree. 2 is the
    /// measured sweet spot. Pages are STRIPED (split) across these, not duplicated.
    pub substreams: usize,
    /// Initial per-substream credit window, in pages (~3.6MB each). DeepSeek: 2.
    pub init_credit: f64,
    /// Max per-substream in-flight, in pages. DeepSeek: 4 (~14.4MB ≈ BDP).
    pub max_credit: f64,
    /// Min per-substream credit (floor on multiplicative decrease), in pages.
    pub min_credit: f64,
    /// Multiplicative-decrease factor applied to cwnd on a stall/loss. DeepSeek: 0.7.
    pub md_factor: f64,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            substreams: 2,
            init_credit: 2.0,
            max_credit: 4.0,
            min_credit: 1.0,
            md_factor: 0.7,
        }
    }
}

#[derive(Debug)]
struct SubState {
    /// Congestion window in pages (fractional; AIMD).
    cwnd: f64,
    /// Pages sent on this substream but not yet ACKed (in send order).
    inflight: Vec<PageId>,
}

/// Deterministic scheduler for ACK-paced striped streaming with retransmit.
///
/// Drives independently of any IO: feed it ACKs/stalls, ask it for the next
/// sends. A real loop pairs it with a [`ChunkSink`] + [`RateGate`] (see
/// [`drive_round`]); tests drive it directly against a simulated link.
#[derive(Debug)]
pub struct ServeStreamPlanner {
    cfg: StreamConfig,
    subs: Vec<SubState>,
    /// Pages not yet in flight or acked (includes retransmits, FIFO).
    queue: VecDeque<PageId>,
    /// Completed (ACKed) page ids — also the dedup set the client mirrors.
    acked: PageBitmap,
    total_pages: u64,
    /// round-robin cursor so striping spreads evenly across substreams.
    rr: usize,
}

impl ServeStreamPlanner {
    /// New planner for `total_pages` pages `[0..total_pages)` (page id = index;
    /// the caller maps id → height range = `base + id*page_records`).
    pub fn new(total_pages: u64, cfg: StreamConfig) -> Self {
        let n = cfg.substreams.max(1);
        let subs = (0..n)
            .map(|_| SubState { cwnd: cfg.init_credit.max(cfg.min_credit), inflight: Vec::new() })
            .collect();
        let queue = (0..total_pages).collect();
        Self {
            cfg,
            subs,
            queue,
            acked: PageBitmap::new(total_pages),
            total_pages,
            rr: 0,
        }
    }

    /// All pages delivered.
    pub fn is_complete(&self) -> bool {
        self.acked.count() == self.total_pages
    }

    /// Pages ACKed so far.
    pub fn delivered(&self) -> u64 {
        self.acked.count()
    }

    /// Current credit window of substream `sub` (pages), for diagnostics/tests.
    pub fn cwnd(&self, sub: usize) -> f64 {
        self.subs[sub].cwnd
    }

    /// In-flight page count on substream `sub`.
    pub fn inflight(&self, sub: usize) -> usize {
        self.subs[sub].inflight.len()
    }

    /// Greedily assign queued pages to substreams that have spare credit
    /// (`inflight < floor(cwnd)`), round-robin so the stripe spreads evenly.
    /// Each returned page is placed on exactly ONE substream (split, not
    /// duplicated). Returns the sends to hand to the [`ChunkSink`].
    pub fn next_sends(&mut self) -> Vec<Send> {
        let mut sends = Vec::new();
        let n = self.subs.len();
        // Keep sweeping while at least one substream can accept and work remains.
        loop {
            if self.queue.is_empty() {
                break;
            }
            let mut progressed = false;
            for _ in 0..n {
                let sub = self.rr % n;
                self.rr = self.rr.wrapping_add(1);
                let cap = self.subs[sub].cwnd.floor().max(1.0) as usize;
                if self.subs[sub].inflight.len() < cap {
                    if let Some(page_id) = self.queue.pop_front() {
                        self.subs[sub].inflight.push(page_id);
                        sends.push(Send { sub, page_id });
                        progressed = true;
                        if self.queue.is_empty() {
                            break;
                        }
                    }
                }
            }
            if !progressed {
                break; // every substream is at its window; wait for ACKs
            }
        }
        sends
    }

    /// A page was ACKed on `sub`. Frees a slot, marks delivery, and applies AIMD
    /// additive-increase (`cwnd += 1/cwnd`, capped at `max_credit`). Idempotent
    /// for duplicate ACKs of an already-delivered page.
    pub fn on_ack(&mut self, sub: usize, page_id: PageId) {
        if let Some(pos) = self.subs[sub].inflight.iter().position(|&p| p == page_id) {
            self.subs[sub].inflight.remove(pos);
        }
        if !self.acked.get(page_id) {
            self.acked.set(page_id);
            let s = &mut self.subs[sub];
            s.cwnd = (s.cwnd + 1.0 / s.cwnd).min(self.cfg.max_credit);
        }
    }

    /// Substream `sub` stalled (RTT timeout / loss signal): multiplicative
    /// decrease and requeue its in-flight pages for retransmit (possibly on a
    /// healthier substream). Already-ACKed pages are skipped.
    pub fn on_stall(&mut self, sub: usize) {
        let s = &mut self.subs[sub];
        s.cwnd = (s.cwnd * self.cfg.md_factor).max(self.cfg.min_credit);
        let requeue: Vec<PageId> = s.inflight.drain(..).collect();
        for p in requeue {
            if !self.acked.get(p) {
                self.queue.push_front(p);
            }
        }
    }

    /// Borrow the delivered-page bitmap (the server's mirror of the client's
    /// dedup set) — for retransmit accounting and tests.
    pub fn acked(&self) -> &PageBitmap {
        &self.acked
    }
}

/// Outcome of one [`drive_round`]: how many pages were pushed, and (if the rate
/// gate throttled us) how long the caller should yield before the next round.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoundOutcome {
    /// Pages actually handed to the sink this round.
    pub sent: usize,
    /// Nanoseconds the caller should wait before re-driving (0 = none). NON-BLOCKING
    /// by contract: this fn never sleeps — the single-threaded responder reactor
    /// also runs block production + gossip serve and must not be stalled. The caller
    /// does `tokio::time::sleep_until(now + wait)`; for sub-millisecond waits (likely
    /// at 100k blk/s, where tokio's ~1ms timer granularity would over-throttle)
    /// prefer `tokio::task::yield_now()` and re-drive (DeepSeek 2026-06-26).
    pub throttle_wait_nanos: u64,
}

/// One driven round: schedule sends, gate each block-pack against the global
/// token-bucket spine (acquire-before-serve, so serve can't overrun v7-ingest),
/// push admitted pages to the sink, and record coordinated-omission latency at
/// [`Stage::Fetch`]. Pages the gate refuses are requeued for a later round and
/// the suggested non-blocking yield is returned. `page_records` = blocks/page.
///
/// Faithful to viktor-v7-coord's #654 pattern `if !gate.admit(N) {
/// wait(gate.admit_wait_nanos(N)) }`, but the wait is RETURNED (never slept here)
/// so the reactor keeps producing.
pub fn drive_round<S, G, C>(
    planner: &mut ServeStreamPlanner,
    sink: &mut S,
    gate: &G,
    clock: &C,
    page_records: u32,
) -> RoundOutcome
where
    S: ChunkSink,
    G: RateGate + CoRecorder,
    C: Clock + ?Sized,
{
    let planned = planner.next_sends();
    if planned.is_empty() {
        return RoundOutcome::default();
    }
    let intended_ns = clock.now_nanos(); // when we WANTED to serve (CO baseline)
    let mut out = RoundOutcome::default();
    let mut throttled = false;
    for s in planned {
        if throttled || !gate.admit(page_records) {
            if !throttled {
                out.throttle_wait_nanos = gate.admit_wait_nanos(page_records);
                throttled = true;
            }
            planner.requeue(s);
            continue;
        }
        let start = clock.now_nanos();
        match sink.send_chunk(s.sub, s.page_id) {
            Ok(()) => {
                let done = clock.now_nanos();
                // CO latency = (start - intended) queue wait + (done - start) service.
                gate.record_fetch(intended_ns, start, done.saturating_sub(start));
                out.sent += 1;
            }
            Err(_) => planner.on_stall(s.sub), // dead substream → MD + requeue
        }
    }
    out
}

impl ServeStreamPlanner {
    /// Return a previously-planned-but-not-sent page to the queue, clearing its
    /// in-flight reservation (used by [`drive_round`] when the rate gate trims a
    /// round). Front of queue so order is preserved.
    pub fn requeue(&mut self, s: Send) {
        if let Some(pos) = self.subs[s.sub].inflight.iter().position(|&p| p == s.page_id) {
            self.subs[s.sub].inflight.remove(pos);
        }
        if !self.acked.get(s.page_id) {
            self.queue.push_front(s.page_id);
        }
    }
}

/// Transport seam: send page `page_id` on substream `sub`. Implemented by
/// rocky-sync-A's `sigil-net` over WG substreams. The recommended on-wire frame
/// prefixes the codec=2 `'S'` payload with `page_id:u64 LE` so the client can
/// dedup with a [`PageBitmap`]; the ACK frame is `(sub:u8, page_id:u64 LE)`.
/// `Err` means the substream is dead → the planner does MD + retransmit.
pub trait ChunkSink {
    fn send_chunk(&mut self, sub: usize, page_id: PageId) -> Result<(), StreamError>;
}

/// Coordinated-omission latency recorder for the serve stage. A LOCAL trait, so
/// we may implement it for the foreign `sigil_synctune::BackpressureSpine` (which
/// records at [`Stage::Fetch`]) as well as the no-op [`AlwaysAdmit`].
/// [`drive_round`] calls it once per served page with (intended, actual_start,
/// service) so the spine's p99 reflects queue wait, not just service time.
pub trait CoRecorder {
    fn record_fetch(&self, intended_ns: u64, actual_start_ns: u64, service_ns: u64);
}

impl<C: Clock> CoRecorder for sigil_synctune::BackpressureSpine<C> {
    fn record_fetch(&self, intended_ns: u64, actual_start_ns: u64, service_ns: u64) {
        self.record(Stage::Fetch.idx(), intended_ns, actual_start_ns, service_ns);
    }
}

/// Default gate that admits everything and records nothing — for standalone use
/// and tests before the shared `sigil-synctune` spine is wired in. Real serve
/// loops pass the shared `Arc<BackpressureSpine>` (the ONE spine), so the
/// producer can't overrun v7-ingest's commit/db stages.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysAdmit;
impl RateGate for AlwaysAdmit {
    fn admit(&self, _n: u32) -> bool {
        true
    }
    fn admit_wait_nanos(&self, _n: u32) -> u64 {
        0
    }
    fn admit_rate(&self) -> u32 {
        sigil_synctune::TARGET_BLK_S
    }
}
impl CoRecorder for AlwaysAdmit {
    fn record_fetch(&self, _: u64, _: u64, _: u64) {}
}

/// Serve fan-out degree ("redundancy") for a measured loss rate, per the
/// flux-chronos sweep baked into sigil-synctune's controller (2026-06-26):
/// redundancy=2 holds ≥99.7% delivery up to 5% loss; escalate to 3 only above 5%
/// (replication cost rises ~linearly — don't over-replicate). v7-autotune drives
/// this online via `KnobSet::serve_redundancy`; this mirrors the rule for static
/// [`StreamConfig`] and matches `StreamConfig::default().substreams == 2`.
pub fn redundancy_for_loss(loss_pct: f64) -> usize {
    if loss_pct > 5.0 {
        3
    } else {
        2
    }
}

/// Compact page-completion bitset. The server tracks delivered pages; the client
/// mirrors it to dedup retransmits (DeepSeek: ~6.25KB for a 50k-bit page set, but
/// here one bit per *page*, far smaller). `first_unset` drives retransmit choice.
#[derive(Debug, Clone)]
pub struct PageBitmap {
    words: Vec<u64>,
    len: u64,
    set_count: u64,
}

impl PageBitmap {
    pub fn new(len: u64) -> Self {
        let words = ((len + 63) / 64) as usize;
        Self { words: vec![0u64; words], len, set_count: 0 }
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, idx: u64) -> bool {
        if idx >= self.len {
            return false;
        }
        (self.words[(idx / 64) as usize] >> (idx % 64)) & 1 == 1
    }
    /// Set bit `idx`; returns true if it was newly set.
    pub fn set(&mut self, idx: u64) -> bool {
        if idx >= self.len || self.get(idx) {
            return false;
        }
        self.words[(idx / 64) as usize] |= 1 << (idx % 64);
        self.set_count += 1;
        true
    }
    pub fn count(&self) -> u64 {
        self.set_count
    }
    /// Lowest index not yet set (the next gap to retransmit), or None if full.
    pub fn first_unset(&self) -> Option<u64> {
        for (w, &word) in self.words.iter().enumerate() {
            if word != u64::MAX {
                let bit = (!word).trailing_zeros() as u64;
                let idx = w as u64 * 64 + bit;
                if idx < self.len {
                    return Some(idx);
                }
            }
        }
        None
    }
}

/// Errors from the transport seam.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("substream {0} closed")]
    SubstreamClosed(usize),
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulated link with per-substream loss; ACKs feed straight back into the
    /// planner. Deterministic pseudo-loss (no Math.random) keyed by page+sub.
    struct SimLink {
        loss_every: u64, // drop 1 in N (0 = lossless)
        delivered: Vec<Send>,
    }
    impl SimLink {
        fn new(loss_every: u64) -> Self {
            Self { loss_every, delivered: Vec::new() }
        }
        fn drops(&self, s: &Send) -> bool {
            self.loss_every != 0 && (s.page_id.wrapping_mul(31).wrapping_add(s.sub as u64)) % self.loss_every == 0
        }
    }
    impl ChunkSink for SimLink {
        fn send_chunk(&mut self, sub: usize, page_id: PageId) -> Result<(), StreamError> {
            self.delivered.push(Send { sub, page_id });
            Ok(())
        }
    }

    #[test]
    fn never_exceeds_per_substream_window() {
        let mut p = ServeStreamPlanner::new(1000, StreamConfig::default());
        let _ = p.next_sends();
        for sub in 0..StreamConfig::default().substreams {
            let cap = p.cwnd(sub).floor().max(1.0) as usize;
            assert!(p.inflight(sub) <= cap, "sub {sub} over window");
        }
    }

    #[test]
    fn pages_are_striped_not_duplicated() {
        let mut p = ServeStreamPlanner::new(50, StreamConfig::default());
        let sends = p.next_sends();
        let mut seen = std::collections::HashSet::new();
        for s in &sends {
            assert!(seen.insert(s.page_id), "page {} sent twice (should be split)", s.page_id);
        }
    }

    #[test]
    fn cwnd_grows_on_ack_and_shrinks_on_stall() {
        let mut p = ServeStreamPlanner::new(100, StreamConfig::default());
        let start = p.cwnd(0);
        let sends = p.next_sends();
        let first = sends.iter().find(|s| s.sub == 0).copied().unwrap();
        p.on_ack(0, first.page_id);
        assert!(p.cwnd(0) > start, "cwnd should grow on ACK");

        let before = p.cwnd(0);
        p.on_stall(0);
        assert!(p.cwnd(0) < before, "cwnd should shrink on stall");
        assert!(p.cwnd(0) >= StreamConfig::default().min_credit);
    }

    #[test]
    fn completes_lossless() {
        let mut p = ServeStreamPlanner::new(200, StreamConfig::default());
        let mut link = SimLink::new(0);
        let mut guard = 0;
        while !p.is_complete() {
            let sends = drive_round_collect(&mut p, &mut link);
            for s in sends {
                p.on_ack(s.sub, s.page_id);
            }
            guard += 1;
            assert!(guard < 10_000, "did not converge");
        }
        assert_eq!(p.delivered(), 200);
    }

    #[test]
    fn completes_with_2pct_loss_via_retransmit() {
        let mut p = ServeStreamPlanner::new(500, StreamConfig::default());
        let mut link = SimLink::new(50); // ~2% loss
        let mut guard = 0;
        while !p.is_complete() {
            let planned = p.next_sends();
            if planned.is_empty() {
                // everything in flight was lost; stall the subs to trigger retransmit
                for sub in 0..StreamConfig::default().substreams {
                    if p.inflight(sub) > 0 {
                        p.on_stall(sub);
                    }
                }
            }
            for s in planned {
                if link.drops(&s) {
                    // lost: the page stays in-flight; a later stall requeues it
                    p.on_stall(s.sub);
                } else {
                    let _ = link.send_chunk(s.sub, s.page_id);
                    p.on_ack(s.sub, s.page_id);
                }
            }
            guard += 1;
            assert!(guard < 100_000, "did not converge under loss");
        }
        assert_eq!(p.delivered(), 500, "every page delivered despite loss");
    }

    #[test]
    fn drive_round_paces_against_spine_and_records_co_latency() {
        use sigil_synctune::{BackpressureSpine, Stage, VirtualClock};
        use std::sync::Arc;
        let clk = Arc::new(VirtualClock::new());
        // 100k blk/s, burst = 20 blocks, COUNT pipeline stages.
        let spine = BackpressureSpine::new(clk.clone(), 100_000, 20, Stage::COUNT);
        let mut p = ServeStreamPlanner::new(100, StreamConfig::default());
        let mut link = SimLink::new(0);
        // page_records = 10 → the 20-block burst admits exactly 2 pages, then throttles.
        let out = drive_round(&mut p, &mut link, &spine, clk.as_ref(), 10);
        assert_eq!(out.sent, 2, "20-block burst = 2 pages of 10 blocks");
        assert!(out.throttle_wait_nanos > 0, "must report a non-blocking yield once drained");

        // Refill by advancing virtual time; finish through the SAME shared spine.
        let mut guard = 0;
        while !p.is_complete() {
            for s in link.delivered.drain(..) {
                p.on_ack(s.sub, s.page_id);
            }
            clk.advance_ms(5); // 100k blk/s * 5ms = 500 blocks refilled
            let _ = drive_round(&mut p, &mut link, &spine, clk.as_ref(), 10);
            guard += 1;
            assert!(guard < 10_000, "no convergence under paced gate");
        }
        for s in link.delivered.drain(..) {
            p.on_ack(s.sub, s.page_id);
        }
        assert_eq!(p.delivered(), 100);
    }

    #[test]
    fn redundancy_escalates_only_above_5pct_loss() {
        assert_eq!(redundancy_for_loss(0.0), 2);
        assert_eq!(redundancy_for_loss(5.0), 2);
        assert_eq!(redundancy_for_loss(5.1), 3);
        assert_eq!(redundancy_for_loss(12.0), 3);
    }

    #[test]
    fn page_bitmap_basics() {
        let mut bm = PageBitmap::new(130);
        assert_eq!(bm.first_unset(), Some(0));
        for i in 0..130 {
            assert!(bm.set(i));
        }
        assert!(!bm.set(5), "double-set returns false");
        assert_eq!(bm.count(), 130);
        assert_eq!(bm.first_unset(), None);
        let mut bm2 = PageBitmap::new(64);
        bm2.set(0);
        bm2.set(1);
        assert_eq!(bm2.first_unset(), Some(2));
    }

    /// helper: schedule + "deliver" within a default (lossless) link, returning
    /// the sends so the test can ACK them.
    fn drive_round_collect(p: &mut ServeStreamPlanner, link: &mut SimLink) -> Vec<Send> {
        let planned = p.next_sends();
        let mut out = Vec::new();
        for s in planned {
            let _ = link.send_chunk(s.sub, s.page_id);
            out.push(s);
        }
        out
    }
}
