//! Global token-bucket backpressure spine + coordinated-omission latency.
//!
//! Every pipeline stage acquires block-permits from one shared [`BackpressureSpine`] before
//! processing blocks. This is the cross-lane piece DeepSeek flagged as missing: without a
//! single rate spine the producer (V7-SUPPLY) overruns ingest (V7-INGEST) -> OOM, and the
//! controller (V7-AUTOTUNE) oscillates on laggy feedback.
//!
//! The bucket lazily refills from the injected [`Clock`] (no background task, deterministic).
//! Tokens are tracked in milli-units so sub-block-per-nanosecond refill is exact integer math.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::Clock;

const MILLI: i64 = 1000;
/// Fixed-point shift for the per-nanosecond refill rate.
const FP: u32 = 20;

struct BucketState {
    milli_tokens: i64,
    capacity_milli: i64,
    /// milli-tokens added per nanosecond, fixed-point scaled by `1 << FP`.
    refill_fp: i128,
    last_ns: u64,
}

/// The shared rate limiter every pipeline stage acquires from.
pub struct BackpressureSpine<C: Clock> {
    clock: Arc<C>,
    state: Mutex<BucketState>,
    rate_blk_s: AtomicU64,
    /// One coordinated-omission histogram per stage id (0..stages).
    latency: Vec<CoLatency>,
}

impl<C: Clock> BackpressureSpine<C> {
    /// `rate_blk_s` = sustained blocks/sec. `burst_blocks` = max instantaneous burst.
    /// `stages` = pipeline stages tracked for latency (fetch/verify/commit/ingest = 4).
    pub fn new(clock: Arc<C>, rate_blk_s: u32, burst_blocks: u32, stages: usize) -> Self {
        let now = clock.now_nanos();
        let cap = burst_blocks.max(1) as i64 * MILLI;
        let state = BucketState {
            milli_tokens: cap,
            capacity_milli: cap,
            refill_fp: Self::refill_for(rate_blk_s),
            last_ns: now,
        };
        let latency = (0..stages.max(1)).map(|_| CoLatency::new()).collect();
        Self {
            clock,
            state: Mutex::new(state),
            rate_blk_s: AtomicU64::new(rate_blk_s as u64),
            latency,
        }
    }

    fn refill_for(rate_blk_s: u32) -> i128 {
        // milli-tokens per ns = rate * MILLI / 1e9, fixed-point scaled by 1<<FP.
        // Round UP (ceil): the spine must be *able* to reach the target rate. Floor division
        // would leave it ~1 block/s short of target forever (it could never sustain 100k).
        // Over-admission from ceil is < 0.001% — negligible for a sync rate limiter.
        let num = ((rate_blk_s as i128) * (MILLI as i128)) << FP;
        (num + 999_999_999) / 1_000_000_000
    }

    fn refill_locked(&self, st: &mut BucketState, now: u64) {
        if now > st.last_ns {
            let dt = (now - st.last_ns) as i128;
            let added = (dt * st.refill_fp) >> FP;
            let next = (st.milli_tokens as i128 + added).min(st.capacity_milli as i128);
            st.milli_tokens = next as i64;
            st.last_ns = now;
        }
    }

    /// Try to acquire `n` block-permits without blocking. Returns true if granted.
    /// Intended usage: acquire in batches (e.g. one commit batch of 128), so the Mutex is
    /// hit ~rate/128 times/sec, not once per block.
    pub fn try_acquire(&self, n: u32) -> bool {
        let need = n as i64 * MILLI;
        let now = self.clock.now_nanos();
        let mut st = self.state.lock().unwrap();
        self.refill_locked(&mut st, now);
        if st.milli_tokens >= need {
            st.milli_tokens -= need;
            true
        } else {
            false
        }
    }

    /// Nanoseconds until `n` permits would be available (0 now, u64::MAX if rate is 0).
    pub fn wait_nanos(&self, n: u32) -> u64 {
        let need = n as i64 * MILLI;
        let now = self.clock.now_nanos();
        let mut st = self.state.lock().unwrap();
        self.refill_locked(&mut st, now);
        if st.milli_tokens >= need {
            return 0;
        }
        if st.refill_fp == 0 {
            return u64::MAX;
        }
        let deficit = (need - st.milli_tokens) as i128;
        (((deficit) << FP) / st.refill_fp) as u64
    }

    /// AIMD entry point: change the sustained rate at runtime (controller drives this).
    pub fn set_rate(&self, rate_blk_s: u32) {
        let now = self.clock.now_nanos();
        let mut st = self.state.lock().unwrap();
        self.refill_locked(&mut st, now);
        st.refill_fp = Self::refill_for(rate_blk_s);
        self.rate_blk_s.store(rate_blk_s as u64, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u32 {
        self.rate_blk_s.load(Ordering::Relaxed) as u32
    }

    /// Record a coordinated-omission sample for `stage`: when work was *intended* to start
    /// vs when it *actually* started, plus how long service took. The recorded latency is the
    /// CO-corrected total (queueing delay + service), which is what hides under load.
    pub fn record(&self, stage: usize, intended_ns: u64, actual_start_ns: u64, service_ns: u64) {
        if let Some(h) = self.latency.get(stage) {
            let queueing = actual_start_ns.saturating_sub(intended_ns);
            h.record(queueing.saturating_add(service_ns));
        }
    }

    pub fn p99_ns(&self, stage: usize) -> u64 {
        self.latency.get(stage).map(|h| h.percentile(0.99)).unwrap_or(0)
    }
    pub fn p50_ns(&self, stage: usize) -> u64 {
        self.latency.get(stage).map(|h| h.percentile(0.50)).unwrap_or(0)
    }
}

/// Dependency-free log-bucketed latency histogram. 64 power-of-two buckets over nanoseconds.
pub struct CoLatency {
    buckets: [AtomicU64; 64],
    count: AtomicU64,
}

impl CoLatency {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
        }
    }
    #[inline]
    fn bucket_of(ns: u64) -> usize {
        if ns == 0 {
            0
        } else {
            (63 - ns.leading_zeros()) as usize
        }
    }
    pub fn record(&self, ns: u64) {
        let b = Self::bucket_of(ns);
        self.buckets[b].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    /// Approximate percentile: representative value is the midpoint of the chosen bucket.
    pub fn percentile(&self, p: f64) -> u64 {
        let total = self.count.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let target = (p * total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            cum += b.load(Ordering::Relaxed);
            if cum >= target {
                let lo = 1u64 << i;
                return lo + (lo >> 1); // midpoint of [2^i, 2^(i+1))
            }
        }
        u64::MAX
    }
}

impl Default for CoLatency {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable stage indices so every lane uses the same id with [`BackpressureSpine::record`] /
/// [`BackpressureSpine::p99_ns`]. Construct the spine with `Stage::COUNT` stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    Fetch = 0,
    Verify = 1,
    Commit = 2,
    Ingest = 3,
}
impl Stage {
    pub const COUNT: usize = 4;
    pub fn idx(self) -> usize {
        self as usize
    }
}

/// Minimal admission interface the serve/ingest lanes code against, so they don't have to name
/// the concrete clock type. `BackpressureSpine<C>` implements it; pass it around as
/// `Arc<dyn RateGate>`. (sigil-serve's manifest references this by name.)
pub trait RateGate: Send + Sync {
    /// Admit `n` blocks if tokens are available now; returns true on success.
    fn admit(&self, n: u32) -> bool;
    /// Nanoseconds until `n` blocks would be admitted (0 = now).
    fn admit_wait_nanos(&self, n: u32) -> u64;
    /// Current admit rate (blocks/sec).
    fn admit_rate(&self) -> u32;
}

impl<C: Clock> RateGate for BackpressureSpine<C> {
    fn admit(&self, n: u32) -> bool {
        self.try_acquire(n)
    }
    fn admit_wait_nanos(&self, n: u32) -> u64 {
        self.wait_nanos(n)
    }
    fn admit_rate(&self) -> u32 {
        self.rate()
    }
}
