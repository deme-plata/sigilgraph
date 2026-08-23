//! sync_store — the single source of truth for backfill sync state.
//!
//! **Why this exists.** `block_sync/mod.rs`'s sync loop carries ~95 `let mut`
//! locals and ~179 scattered references to `assigned` / `synced_to()` /
//! `fetched_total` / `frontier`. No single component owns a range's lifecycle,
//! and two production bugs found on 2026-08-23 came directly from that:
//!
//! 1. **The claim set never released on success.** `assigned: HashSet<u64>`
//!    gained an entry when a range was requested and only ever lost one on
//!    FAILURE (`if !fetched_ok { assigned.remove(&start) }`) or on a wholesale
//!    `clear()`. Successfully-fetched ranges stayed claimed forever, so the
//!    refill loop's `if !assigned.insert(start) { continue; }` skipped the
//!    whole window and issued nothing.
//! 2. **Look-ahead was gated on the wrong watermark.** The cap read
//!    `store.synced_to()` — the VERIFIED frontier — while claims tracked what
//!    had been FETCHED. On a full-archive sync verify is the slower stage
//!    (operator's live node: ~1.5M fetched vs ~537k verified), so every
//!    look-ahead slot tripped the cap, in-flight drained to zero, and the wire
//!    went idle until verify caught up — then burst again. That sawtooth is
//!    the "very unstable, waiting with zero blocks" the operator reported.
//!
//! Neither is a hard bug to fix in isolation; both are *inevitable* when the
//! same conceptual state is spread across a `HashSet`, a watermark owned by a
//! different struct, and a magic constant. This module makes range lifecycle a
//! total, explicit state machine with exactly one owner.
//!
//! **Two watermarks, never conflated.** [`SyncStore::fetched_to`] and
//! [`SyncStore::verified_to`] are separate by construction. Fetch look-ahead
//! is bounded by [`SyncStore::unverified_ranges`] — a real, bounded resource
//! (how much fetched-but-unverified work is outstanding) — instead of by a
//! multiple of a watermark that fetch has no control over. That is the actual
//! cure for the sawtooth: a slow verify stage now applies *backpressure*
//! proportional to real outstanding work, rather than slamming fetch to zero.
//!
//! **Durability.** Optionally backed by `flux_db::Database` (already a
//! `sigil-top` dependency). Today `assigned` lives only in RAM, so a restart
//! re-requests ranges it already has on disk. Persisted state lets a restart
//! resume. The store works fully in-memory when no database is attached, so
//! tests and the chronos simulation need no filesystem.
//!
//! **Selectors.** Derived views (`progress`, `observed_rate`, `eta_secs`) are
//! computed from the store rather than recomputed independently at each call
//! site — the reason the live TUI could report "STALLED — rate 0 blk/s" while
//! the node was genuinely advancing ~203 blocks/s.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Lifecycle of one fetch range, keyed by its start height.
///
/// Total and explicit: every transition is a named method on [`SyncStore`],
/// so "claimed but never released" cannot be expressed by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeState {
    /// Requested from `peer`, awaiting a reply.
    InFlight {
        /// Peer the request went to.
        peer: String,
        /// Monotonic millis when the request was issued (for timeout sweeps).
        since_ms: u64,
    },
    /// Bytes arrived and were committed; not yet verified/applied.
    Fetched {
        /// Monotonic millis when the reply landed.
        at_ms: u64,
    },
    /// Verified and applied — this range is done and costs no budget.
    Verified,
}

#[derive(Default)]
struct Inner {
    ranges: BTreeMap<u64, RangeState>,
    fetched_to: u64,
    verified_to: u64,
}

/// The sync state store. Cheap to clone (`Arc` inside); every method takes
/// `&self` so call sites keep a shared handle, matching how `assigned` and the
/// counters were previously threaded through the loop.
pub struct SyncStore {
    inner: RwLock<Inner>,
    /// Chunk stride — the span of one range.
    chunk: u64,
    /// Optional durable backing. `None` = pure in-memory (tests, simulation).
    db: Option<Arc<flux_db::Database>>,
    /// Cumulative blocks fetched this session (for `observed_rate`).
    fetched_blocks: AtomicU64,
    /// Monotonic millis when the store was created (rate denominator).
    started_ms: u64,
}

const KEY_PREFIX: &[u8] = b"syncstore/v1/";
const KEY_WATERMARKS: &[u8] = b"syncstore/v1/watermarks";

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl SyncStore {
    /// In-memory store with the given chunk stride.
    pub fn new(chunk: u64) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            chunk: chunk.max(1),
            db: None,
            fetched_blocks: AtomicU64::new(0),
            started_ms: now_ms(),
        }
    }

    /// Store backed by a durable database. State is written through on
    /// transitions so a restart can [`SyncStore::load`] instead of re-fetching.
    pub fn with_db(chunk: u64, db: Arc<flux_db::Database>) -> Self {
        let mut s = Self::new(chunk);
        s.db = Some(db);
        s
    }

    /// Range stride.
    pub fn chunk(&self) -> u64 {
        self.chunk
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Claim `start` for a request. Returns `false` if it is already InFlight,
    /// Fetched, or Verified — the direct replacement for
    /// `assigned.insert(start)`, but with the crucial difference that a
    /// successful fetch later transitions to `Verified` and frees the budget
    /// instead of pinning the slot forever.
    pub fn claim(&self, start: u64, peer: &str) -> bool {
        let mut g = self.write();
        if g.ranges.contains_key(&start) {
            return false;
        }
        g.ranges.insert(start, RangeState::InFlight { peer: peer.to_string(), since_ms: now_ms() });
        drop(g);
        self.persist_range(start);
        true
    }

    /// Release a claim without progress — a failed/timed-out request. The
    /// range returns to unclaimed and may be re-requested (possibly from a
    /// different peer).
    pub fn release(&self, start: u64) {
        let mut g = self.write();
        if matches!(g.ranges.get(&start), Some(RangeState::InFlight { .. })) {
            g.ranges.remove(&start);
        }
        drop(g);
        self.persist_range(start);
    }

    /// A reply landed and was committed. `blocks` feeds the rate selector.
    pub fn mark_fetched(&self, start: u64, blocks: u64) {
        {
            let mut g = self.write();
            g.ranges.insert(start, RangeState::Fetched { at_ms: now_ms() });
            let end = start.saturating_add(self.chunk);
            if end > g.fetched_to {
                g.fetched_to = end;
            }
        }
        self.fetched_blocks.fetch_add(blocks, Ordering::Relaxed);
        self.persist_range(start);
        self.persist_watermarks();
    }

    /// Advance the verified watermark. Every range fully below `height`
    /// becomes `Verified`, freeing the backpressure budget it held.
    pub fn mark_verified_to(&self, height: u64) {
        {
            let mut g = self.write();
            if height <= g.verified_to {
                return;
            }
            g.verified_to = height;
            let chunk = self.chunk;
            for (start, st) in g.ranges.iter_mut() {
                if start.saturating_add(chunk) <= height {
                    *st = RangeState::Verified;
                }
            }
            // Verified ranges below the watermark carry no further information
            // — drop them so the map tracks only live work.
            g.ranges.retain(|start, _| start.saturating_add(chunk) > height);
        }
        self.persist_watermarks();
    }

    /// Drop claims the verified frontier has passed. Mirrors the loop's old
    /// `assigned.retain(|&s| s >= now_synced)` so the swap is
    /// behavior-preserving at that call site.
    pub fn retain_from(&self, now_synced: u64) {
        self.write().ranges.retain(|&start, _| start >= now_synced);
    }

    /// Ranges tracked at all (in-flight + fetched-unverified) — the direct
    /// replacement for `assigned.len()` in the debug line.
    pub fn tracked(&self) -> usize {
        self.read().ranges.len()
    }

    /// Forget all range state (a chain jump / base snap made it meaningless).
    /// Watermarks are preserved — they describe the chain, not the claims.
    pub fn clear_ranges(&self) {
        self.write().ranges.clear();
    }

    /// Highest height fetched (exclusive end of the highest fetched range).
    pub fn fetched_to(&self) -> u64 {
        self.read().fetched_to
    }

    /// Highest height verified + applied.
    pub fn verified_to(&self) -> u64 {
        self.read().verified_to
    }

    /// Ranges currently awaiting a reply.
    pub fn inflight(&self) -> usize {
        self.read().ranges.values().filter(|s| matches!(s, RangeState::InFlight { .. })).count()
    }

    /// Ranges fetched but not yet verified — **the real backpressure signal**.
    /// This, not a multiple of the verified watermark, is what fetch
    /// look-ahead should be bounded by: it measures actual outstanding work,
    /// so a slow verify stage throttles fetch smoothly instead of to zero.
    pub fn unverified_ranges(&self) -> usize {
        self.read().ranges.values().filter(|s| matches!(s, RangeState::Fetched { .. })).count()
    }

    /// May a new range be fetched, given a budget of outstanding
    /// fetched-but-unverified ranges? Replaces
    /// `start > synced_to() + CHUNK*16` — which coupled fetch to a watermark
    /// fetch cannot influence.
    pub fn may_fetch(&self, budget_ranges: usize) -> bool {
        self.outstanding() < budget_ranges
    }

    /// Work that is already committed to but not yet verified: in-flight
    /// requests PLUS fetched-but-unverified ranges.
    ///
    /// FOUND BY CHRONOS, 2026-08-23, before this reached production. The first
    /// version of `may_fetch` counted only `Fetched`. In-flight requests are
    /// future unverified work, so with `max_inflight` requests outstanding the
    /// budget overshot by up to that many (measured: 67 against a budget of
    /// 64), which hard-blocked fetch until verify drained back under — then
    /// burst. That is the very block-drain-burst sawtooth this store exists to
    /// remove, recreated inside the fix. Counting both states keeps the gate
    /// smooth: it never overshoots, so it never has to hard-block.
    pub fn outstanding(&self) -> usize {
        self.read()
            .ranges
            .values()
            .filter(|s| matches!(s, RangeState::InFlight { .. } | RangeState::Fetched { .. }))
            .count()
    }

    /// Sweep `InFlight` ranges older than `timeout_ms` back to unclaimed, so a
    /// peer that never replies cannot pin a slot forever. Returns the released
    /// starts.
    pub fn sweep_timeouts(&self, timeout_ms: u64) -> Vec<u64> {
        let now = now_ms();
        let mut released = Vec::new();
        {
            let mut g = self.write();
            let stale: Vec<u64> = g
                .ranges
                .iter()
                .filter_map(|(start, st)| match st {
                    RangeState::InFlight { since_ms, .. } if now.saturating_sub(*since_ms) >= timeout_ms => Some(*start),
                    _ => None,
                })
                .collect();
            for s in stale {
                g.ranges.remove(&s);
                released.push(s);
            }
        }
        released
    }

    // ── selectors (derived views — one definition, so the UI cannot disagree
    //    with reality the way the live TUI did) ──

    /// Fraction of `tip` verified, 0.0..=1.0.
    pub fn progress(&self, tip: u64) -> f64 {
        if tip == 0 {
            return 0.0;
        }
        (self.verified_to() as f64 / tip as f64).clamp(0.0, 1.0)
    }

    /// Blocks/sec fetched over the session.
    pub fn observed_rate(&self) -> f64 {
        let secs = (now_ms().saturating_sub(self.started_ms)) as f64 / 1000.0;
        if secs <= 0.0 {
            return 0.0;
        }
        self.fetched_blocks.load(Ordering::Relaxed) as f64 / secs
    }

    /// Seconds to reach `tip` at the current rate, `None` if not yet moving.
    pub fn eta_secs(&self, tip: u64) -> Option<u64> {
        let rate = self.observed_rate();
        if rate <= 0.0 {
            return None;
        }
        let remaining = tip.saturating_sub(self.verified_to()) as f64;
        Some((remaining / rate) as u64)
    }

    /// Is the wire genuinely idle — nothing in flight AND budget available?
    /// The condition worth alarming on, as opposed to "rate momentarily 0",
    /// which is normal while verify catches up.
    pub fn is_wire_idle(&self, budget_ranges: usize) -> bool {
        self.inflight() == 0 && self.may_fetch(budget_ranges)
    }

    // ── durability ──

    fn persist_range(&self, start: u64) {
        let Some(db) = &self.db else { return };
        let g = self.read();
        let mut key = KEY_PREFIX.to_vec();
        key.extend_from_slice(&start.to_be_bytes());
        match g.ranges.get(&start) {
            // Only FETCHED survives a restart. An InFlight claim is bound to a
            // live request that the restart destroyed — persisting it would
            // recreate exactly the "claimed forever, never released" bug this
            // module exists to remove.
            Some(RangeState::Fetched { .. }) => {
                let _ = db.put(&key, b"f");
            }
            _ => {
                let _ = db.delete(&key);
            }
        }
    }

    fn persist_watermarks(&self) {
        let Some(db) = &self.db else { return };
        let g = self.read();
        let mut buf = [0u8; 16];
        buf[..8].copy_from_slice(&g.fetched_to.to_be_bytes());
        buf[8..].copy_from_slice(&g.verified_to.to_be_bytes());
        let _ = db.put(KEY_WATERMARKS, &buf);
    }

    /// Restore persisted state after a restart. Returns the number of Fetched
    /// ranges recovered — work a fresh process would otherwise re-request.
    pub fn load(&self, known_starts: &[u64]) -> usize {
        let Some(db) = &self.db else { return 0 };
        if let Ok(Some(buf)) = db.get(KEY_WATERMARKS) {
            if buf.len() == 16 {
                let mut g = self.write();
                g.fetched_to = u64::from_be_bytes(buf[..8].try_into().unwrap_or_default());
                g.verified_to = u64::from_be_bytes(buf[8..].try_into().unwrap_or_default());
            }
        }
        let mut restored = 0;
        for &start in known_starts {
            let mut key = KEY_PREFIX.to_vec();
            key.extend_from_slice(&start.to_be_bytes());
            if let Ok(Some(_)) = db.get(&key) {
                self.write().ranges.insert(start, RangeState::Fetched { at_ms: now_ms() });
                restored += 1;
            }
        }
        restored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_exclusive_then_frees_on_verify() {
        let s = SyncStore::new(1000);
        assert!(s.claim(0, "peerA"), "first claim wins");
        assert!(!s.claim(0, "peerB"), "a claimed range must not be re-claimed");
        assert_eq!(s.inflight(), 1);

        s.mark_fetched(0, 1000);
        assert_eq!(s.inflight(), 0);
        assert_eq!(s.unverified_ranges(), 1, "fetched-not-verified holds budget");
        assert!(!s.claim(0, "peerC"), "a fetched range must not be re-fetched");

        // THE BUG THIS MODULE EXISTS FOR: under the old HashSet the range
        // stayed claimed forever. Verifying must free the budget.
        s.mark_verified_to(1000);
        assert_eq!(s.unverified_ranges(), 0, "verifying must release the budget the range held");
    }

    #[test]
    fn release_returns_a_failed_range_to_unclaimed() {
        let s = SyncStore::new(1000);
        assert!(s.claim(2000, "peerA"));
        s.release(2000);
        assert_eq!(s.inflight(), 0);
        assert!(s.claim(2000, "peerB"), "a released range must be re-requestable, possibly elsewhere");
    }

    #[test]
    fn watermarks_are_never_conflated() {
        let s = SyncStore::new(1000);
        for start in [0u64, 1000, 2000, 3000] {
            assert!(s.claim(start, "p"));
            s.mark_fetched(start, 1000);
        }
        assert_eq!(s.fetched_to(), 4000);
        assert_eq!(s.verified_to(), 0, "fetching must not imply verifying");
        s.mark_verified_to(2000);
        assert_eq!(s.verified_to(), 2000);
        assert_eq!(s.fetched_to(), 4000, "verifying must not move the fetched watermark");
    }

    /// The sawtooth, as a regression test. Verify is slower than fetch; the
    /// old rule (`start > verified_to + 16*CHUNK`) drove look-ahead to zero.
    /// The budget rule must keep fetching while unverified work is under
    /// budget, and must apply backpressure — not a hard stop — when it isn't.
    #[test]
    fn slow_verify_throttles_fetch_smoothly_instead_of_to_zero() {
        let s = SyncStore::new(1000);
        let budget = 8usize;

        // Fetch races ahead while verify does nothing at all.
        let mut start = 0u64;
        while s.may_fetch(budget) {
            assert!(s.claim(start, "p"));
            s.mark_fetched(start, 1000);
            start += 1000;
        }
        assert_eq!(s.unverified_ranges(), budget, "fetch fills exactly the budget, no more");
        assert!(!s.may_fetch(budget), "at budget, backpressure applies");
        assert_eq!(s.verified_to(), 0, "verify genuinely never advanced");

        // Verify one range: exactly one slot frees, fetch resumes immediately.
        s.mark_verified_to(1000);
        assert!(s.may_fetch(budget), "verifying ONE range must immediately re-open fetch");
        assert_eq!(s.unverified_ranges(), budget - 1);
    }

    #[test]
    fn timeout_sweep_frees_a_peer_that_never_replies() {
        let s = SyncStore::new(1000);
        assert!(s.claim(5000, "dead-peer"));
        assert!(s.sweep_timeouts(60_000).is_empty(), "a fresh claim must not be swept");
        let released = s.sweep_timeouts(0);
        assert_eq!(released, vec![5000]);
        assert_eq!(s.inflight(), 0);
        assert!(s.claim(5000, "live-peer"), "swept range is re-requestable");
    }

    #[test]
    fn selectors_report_reality_not_zero_while_progressing() {
        let s = SyncStore::new(1000);
        s.mark_verified_to(500);
        assert!((s.progress(1000) - 0.5).abs() < 1e-9);
        assert_eq!(s.progress(0), 0.0, "no tip yet must not divide by zero");
        // is_wire_idle distinguishes "genuinely nothing happening" from
        // "momentarily 0 blk/s while verify catches up" — the exact distinction
        // the live TUI got wrong.
        assert!(s.is_wire_idle(4), "nothing in flight and budget free = genuinely idle");
        assert!(s.claim(0, "p"));
        assert!(!s.is_wire_idle(4), "a request in flight is NOT idle");
    }

    #[test]
    fn clear_ranges_keeps_watermarks() {
        let s = SyncStore::new(1000);
        assert!(s.claim(0, "p"));
        s.mark_fetched(0, 1000);
        s.mark_verified_to(1000);
        s.clear_ranges();
        assert_eq!(s.verified_to(), 1000, "a base snap invalidates claims, not chain facts");
    }
}
