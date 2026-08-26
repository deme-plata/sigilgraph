//! **Coinbase attribution ledger** — where did each block's miner slice actually go?
//!
//! # The bug this exists to make impossible
//!
//! Twice now, SIGIL has quietly paid ~94% of all newly minted supply to the
//! node's own producer wallet while real miners hashed for nothing:
//!
//! * the first time is recorded in `mining::credit_window`'s doc — *"measured
//!   live: 93.8% of the entire supply had gone to the node's placeholder
//!   producer wallet"*;
//! * the second was measured on 2026-08-26, **94%**, on the live chain:
//!
//! ```text
//! wallet             blocks_produced        delta_balance   (90 s window)
//! 73b7745271b6be22               198         +850,126,001   ← the producer wallet
//! 5f749b942de3e96b                 0           +1,495,229   ← a 2.49 GH/s rig
//! 434fe2d28520aed7                 0                   +0   ← four real rigs, ~45 MH/s
//! ```
//!
//! Nothing was *broken* either time. The mint cadence (~5 blk/s) and the
//! target-win cadence (one block-solve per ~120 s) were simply never
//! reconciled, so nearly every block fell through to the producer default.
//!
//! **The reason it survived for months, twice, is that nothing in the node ever
//! reported this.** Every existing metric looked healthy: shares were accepted,
//! blocks were produced, supply climbed, hashrate was up. Finding it required a
//! bespoke off-box experiment — sampling every miner's balance over a 90-second
//! window and diffing it against the supply delta. That is not a thing anyone
//! runs by accident, which is exactly why it took months.
//!
//! A fix for the payout itself is a fix for *today*. This module is the part
//! that stops it coming back: it makes the question **"are miners actually
//! being paid?"** answerable in one call, at any moment, by anyone.
//!
//! # What it records
//!
//! One entry per minted block, in a bounded ring: the height, the reward, how
//! many distinct payees the miner slice was split across, and — the load-bearing
//! field — **why** it went where it went ([`PayoutSource`]). That last one is
//! the difference between "supply is climbing" (always true, tells you nothing)
//! and "supply is climbing *and it is reaching the people doing the work*".
//!
//! # Deliberate non-goals
//!
//! * **Not consensus.** Nothing here is hashed, signed, or gossiped. It is
//!   local observability over blocks this node minted. A node that never calls
//!   [`record`] is byte-for-byte unaffected.
//! * **Not an audit trail.** The ring is bounded and in-memory; it is a
//!   *smoke detector*, not a ledger of record. The chain is the ledger.
//! * **Not per-payee accounting.** It stores payee *counts*, never wallet ids
//!   or amounts per wallet — a per-wallet emission table on a public endpoint
//!   would be a deanonymisation surface on a chain whose entire point is
//!   privacy. The fairness question is answerable without it.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// How many recent blocks the ring keeps. At ~5 blk/s this is ~13 minutes of
/// history — long enough to be statistically meaningful, short enough that the
/// answer reflects the chain as it is *right now* rather than an hour ago.
const RING_CAP: usize = 4096;

/// Fraction of the miner slice going to the producer fallback that is
/// considered a healthy ceiling. Above this, with real miners live, something
/// is wrong — this is the 94% incident's detector threshold.
const FALLBACK_ALARM_PCT: f64 = 50.0;

/// Minimum blocks in the window before the alarm is allowed to fire, so a
/// freshly-booted node doesn't scream about a 3-block sample.
const ALARM_MIN_SAMPLE: usize = 256;

/// Why this block's miner slice went where it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayoutSource {
    /// A miner met the block target. The reward follows a real proof of work —
    /// this is the case the whole design is *for*.
    RealSolve,
    /// No block-target solve, but partial shares had arrived since the last
    /// block, so the reward was split across the miners who proved that work.
    SharePool,
    /// No solve and no shares to pay: the producer wallet took the miner slice
    /// by default. **Healthy in small amounts** (a genuinely idle network has
    /// nobody to pay). Pathological as the steady state — that is the bug.
    ProducerFallback,
}

impl PayoutSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PayoutSource::RealSolve => "real_solve",
            PayoutSource::SharePool => "share_pool",
            PayoutSource::ProducerFallback => "producer_fallback",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    height: u64,
    source: PayoutSource,
    /// Distinct wallets the miner slice was split across (1 for a fallback).
    payees: u32,
    reward: u128,
}

fn ring() -> &'static Mutex<VecDeque<Entry>> {
    static R: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

/// Record one minted block's payout. Cheap, lock-guarded, never fails: a
/// poisoned lock is swallowed rather than propagated, because observability
/// must never be able to take down block production.
pub fn record(height: u64, source: PayoutSource, payees: u32, reward: u128) {
    if let Ok(mut r) = ring().lock() {
        if r.len() == RING_CAP {
            r.pop_front();
        }
        r.push_back(Entry { height, source, payees, reward });
    }
}

/// Aggregate view over the ring.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub blocks: usize,
    pub height_lo: u64,
    pub height_hi: u64,
    pub real_solve_blocks: usize,
    pub share_pool_blocks: usize,
    pub producer_fallback_blocks: usize,
    pub real_solve_value: u128,
    pub share_pool_value: u128,
    pub producer_fallback_value: u128,
    /// Share of emitted VALUE (not block count) that went to the producer
    /// fallback. Value, not count, is the honest denominator — a chain paying
    /// miners on 90% of blocks but the fallback on the 10% that carried most of
    /// the reward is not a healthy chain.
    pub producer_fallback_pct: f64,
    /// Mean distinct payees on blocks that actually paid miners. A number near
    /// 1 on a multi-miner network means payouts are concentrating.
    pub mean_payees_when_paid: f64,
    /// True when the fallback share is above the healthy ceiling on a sample
    /// large enough to mean something.
    pub alarm: bool,
}

/// Summarise the last `window` blocks (clamped to what the ring holds). Pass 0
/// for everything retained.
pub fn summary(window: usize) -> Summary {
    let Ok(r) = ring().lock() else {
        return Summary::default();
    };
    let take = if window == 0 { r.len() } else { window.min(r.len()) };
    if take == 0 {
        return Summary::default();
    }
    let mut s = Summary { blocks: take, height_lo: u64::MAX, ..Default::default() };
    let mut paid_blocks = 0usize;
    let mut paid_payees = 0u64;
    for e in r.iter().skip(r.len() - take) {
        s.height_lo = s.height_lo.min(e.height);
        s.height_hi = s.height_hi.max(e.height);
        match e.source {
            PayoutSource::RealSolve => {
                s.real_solve_blocks += 1;
                s.real_solve_value = s.real_solve_value.saturating_add(e.reward);
                paid_blocks += 1;
                paid_payees += e.payees as u64;
            }
            PayoutSource::SharePool => {
                s.share_pool_blocks += 1;
                s.share_pool_value = s.share_pool_value.saturating_add(e.reward);
                paid_blocks += 1;
                paid_payees += e.payees as u64;
            }
            PayoutSource::ProducerFallback => {
                s.producer_fallback_blocks += 1;
                s.producer_fallback_value = s.producer_fallback_value.saturating_add(e.reward);
            }
        }
    }
    let total = s
        .real_solve_value
        .saturating_add(s.share_pool_value)
        .saturating_add(s.producer_fallback_value);
    s.producer_fallback_pct = if total == 0 {
        0.0
    } else {
        (s.producer_fallback_value as f64) * 100.0 / (total as f64)
    };
    s.mean_payees_when_paid =
        if paid_blocks == 0 { 0.0 } else { paid_payees as f64 / paid_blocks as f64 };
    s.alarm = take >= ALARM_MIN_SAMPLE && s.producer_fallback_pct > FALLBACK_ALARM_PCT;
    s
}

/// One-line human verdict, for a log line or an API field. States the number
/// and what it means, because "94.0%" on its own has already failed to alarm
/// anyone twice.
pub fn verdict(s: &Summary) -> String {
    if s.blocks == 0 {
        return "no blocks minted by this node yet — nothing to attribute".into();
    }
    if s.alarm {
        format!(
            "🚨 {:.1}% of emitted value went to the PRODUCER WALLET over the last {} blocks — \
             real miners are not being paid. This is the 93.8%/94% failure mode; check that a \
             block-target solve is reachable at the current mint cadence, and that the share-pool \
             payout is enabled.",
            s.producer_fallback_pct, s.blocks
        )
    } else {
        format!(
            "{:.1}% of emitted value to the producer fallback over {} blocks \
             ({} real-solve, {} share-pool, {} fallback; mean {:.1} payees when paid)",
            s.producer_fallback_pct,
            s.blocks,
            s.real_solve_blocks,
            s.share_pool_blocks,
            s.producer_fallback_blocks,
            s.mean_payees_when_paid
        )
    }
}

/// Test-only: drop everything retained, so tests don't see each other's writes.
#[cfg(test)]
fn reset() {
    if let Ok(mut r) = ring().lock() {
        r.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share one process-global ring, so they must not run
    /// concurrently against it. One lock, taken by every test.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static G: OnceLock<Mutex<()>> = OnceLock::new();
        G.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn empty_ledger_is_silent_not_alarming() {
        let _g = guard();
        reset();
        let s = summary(0);
        assert_eq!(s.blocks, 0);
        assert!(!s.alarm, "a node that has minted nothing must not alarm");
        assert!(verdict(&s).contains("nothing to attribute"));
    }

    /// The exact live incident: the producer takes essentially everything while
    /// miners take a sliver. This is the case the module exists to catch.
    #[test]
    fn the_94_percent_incident_alarms() {
        let _g = guard();
        reset();
        for h in 0..1000u64 {
            if h % 50 == 0 {
                record(h, PayoutSource::RealSolve, 3, 1_000_000);
            } else {
                record(h, PayoutSource::ProducerFallback, 1, 1_000_000);
            }
        }
        let s = summary(0);
        assert_eq!(s.producer_fallback_blocks, 980);
        assert!(
            (s.producer_fallback_pct - 98.0).abs() < 0.01,
            "got {:.2}%",
            s.producer_fallback_pct
        );
        assert!(s.alarm, "98% to the producer wallet MUST alarm");
        assert!(verdict(&s).contains("not being paid"));
    }

    /// A healthy chain: nearly every block pays real work.
    #[test]
    fn a_healthy_chain_does_not_alarm() {
        let _g = guard();
        reset();
        for h in 0..1000u64 {
            if h % 100 == 0 {
                record(h, PayoutSource::ProducerFallback, 1, 1_000_000);
            } else {
                record(h, PayoutSource::SharePool, 5, 1_000_000);
            }
        }
        let s = summary(0);
        assert!(s.producer_fallback_pct < 2.0, "got {:.2}%", s.producer_fallback_pct);
        assert!(!s.alarm);
        assert!((s.mean_payees_when_paid - 5.0).abs() < 0.01);
    }

    /// VALUE, not block count, is the denominator — a chain that pays miners on
    /// most blocks but hands the fallback the few blocks carrying the money is
    /// NOT healthy, and counting blocks would call it healthy.
    #[test]
    fn value_not_block_count_decides_the_verdict() {
        let _g = guard();
        reset();
        for h in 0..900u64 {
            record(h, PayoutSource::SharePool, 4, 1); // many blocks, negligible value
        }
        for h in 900..1000u64 {
            record(h, PayoutSource::ProducerFallback, 1, 1_000_000); // few blocks, all the money
        }
        let s = summary(0);
        assert_eq!(s.share_pool_blocks, 900, "miners were paid on 90% of BLOCKS");
        assert!(
            s.producer_fallback_pct > 99.0,
            "...but got {:.2}% of the VALUE — must still alarm",
            s.producer_fallback_pct
        );
        assert!(s.alarm);
    }

    #[test]
    fn a_small_sample_never_alarms() {
        let _g = guard();
        reset();
        for h in 0..10u64 {
            record(h, PayoutSource::ProducerFallback, 1, 1_000_000);
        }
        let s = summary(0);
        assert!(
            (s.producer_fallback_pct - 100.0).abs() < 0.01,
            "the percentage is still reported honestly"
        );
        assert!(!s.alarm, "but 10 blocks is not enough evidence to cry bug");
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let _g = guard();
        reset();
        for h in 0..(RING_CAP as u64 + 500) {
            record(h, PayoutSource::RealSolve, 1, 1);
        }
        let s = summary(0);
        assert_eq!(s.blocks, RING_CAP, "memory must stay bounded on a live node");
        assert_eq!(s.height_hi, RING_CAP as u64 + 499, "newest entry retained");
        assert_eq!(s.height_lo, 500, "oldest entries evicted");
    }

    #[test]
    fn window_narrows_to_the_most_recent_blocks() {
        let _g = guard();
        reset();
        for h in 0..600u64 {
            record(h, PayoutSource::ProducerFallback, 1, 1_000_000);
        }
        for h in 600..1000u64 {
            record(h, PayoutSource::SharePool, 2, 1_000_000);
        }
        // The whole ring is mixed...
        assert!(summary(0).producer_fallback_pct > 50.0);
        // ...but the most recent 400 blocks are clean, and the window says so.
        let recent = summary(400);
        assert_eq!(recent.blocks, 400);
        assert_eq!(recent.producer_fallback_pct, 0.0);
        assert!(!recent.alarm);
    }
}
