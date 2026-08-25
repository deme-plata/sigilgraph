//! mining_history.rs — durable time-series storage for network hashrate /
//! miner-count history.
//!
//! `MiningBridge` (see `mining.rs`) is deliberately pure in-memory live
//! state — nothing about past hashrate survives a restart, and there is no
//! way today to ask "what was the network doing 3 days ago". This module is
//! the durable side of that: a `flux-db`-backed append-only time series with
//! real rollup/downsampling, so the wallet's Network Power modal can offer
//! 24h/7d/30d/1y/all without either (a) keeping a year of 1-minute samples
//! forever, or (b) faking the longer ranges from whatever happens to still
//! be in memory.
//!
//! Retention design (deliberately simple, three tiers):
//!   - RAW samples, one per minute: kept 48h (~2,880 rows at steady state).
//!     Covers the 24h view with a full extra day of margin.
//!   - HOURLY rollups (avg + max over each wall-clock hour): kept 60 days
//!     (~1,440 rows at steady state). Covers 7d and 30d.
//!   - DAILY rollups (avg + max over each wall-clock day): kept forever
//!     (~365 rows/year — trivial even at "all time" for a chain that's
//!     years old). Covers 1y and all.
//!
//! A single `Database` (not per-tier column families — mirrors
//! `sigil-block-store`'s proven prefixed-key approach rather than
//! introducing untested CF-reopen semantics) holds all three tiers,
//! namespaced by a 1-byte prefix ahead of the big-endian u64 timestamp key
//! (same trick as `sigil-block-store::height_key`, which is what makes
//! lexicographic key order equal chronological order and lets `iter_from`
//! serve range scans directly).
//!
//! Compaction (raw→hourly, hourly→daily) runs inline on the sampler's own
//! tick (see `spawn_sampler`), gated to at most once per
//! `COMPACTION_INTERVAL_SECS` so a 60s sampling cadence doesn't re-scan the
//! whole raw tier on every single tick.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const PFX_RAW: u8 = 0x01;
const PFX_HOURLY: u8 = 0x02;
const PFX_DAILY: u8 = 0x03;

const RAW_RETENTION_SECS: u64 = 48 * 3600;
const HOURLY_RETENTION_SECS: u64 = 60 * 24 * 3600;
const HOUR_SECS: u64 = 3600;
const DAY_SECS: u64 = 86_400;
/// Don't re-scan the raw/hourly tiers on every sampler tick — compaction is
/// idempotent and cheap either way, but there's no reason to pay the scan
/// cost more than once every few minutes.
const COMPACTION_INTERVAL_SECS: u64 = 300;

fn key(prefix: u8, ts: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(prefix);
    k.extend_from_slice(&ts.to_be_bytes());
    k
}

fn key_range_start(prefix: u8) -> Vec<u8> {
    key(prefix, 0)
}

/// One durable sample point as served to the frontend — used directly for
/// the raw (1-minute) tier, and as the query-time output shape for the
/// rolled-up tiers too (an hourly/daily row's `hashrate`/`miners` are that
/// bucket's average — see [`Agg::avg_hashrate`]/[`Agg::avg_miners`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct HistoryPoint {
    pub timestamp: u64,
    pub hashrate: f64,
    pub miners: u32,
}

/// Rollup accumulator stored in the hourly/daily tiers. Keeps both avg and
/// max (max is cheap to keep and the more interesting number for "was there
/// a hashrate spike this hour" — avg alone would smooth it away), and `n` so
/// merging an aged-out hourly batch into an already-populated daily bucket
/// (the normal steady-state case once the store has been running a while)
/// re-weights correctly instead of averaging-of-averages.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
struct Agg {
    sum_hashrate: f64,
    max_hashrate: f64,
    sum_miners: f64,
    max_miners: u32,
    n: u64,
}

impl Agg {
    fn from_point(hashrate: f64, miners: u32) -> Self {
        Self { sum_hashrate: hashrate, max_hashrate: hashrate, sum_miners: miners as f64, max_miners: miners, n: 1 }
    }
    fn merge(&mut self, other: &Agg) {
        self.sum_hashrate += other.sum_hashrate;
        self.max_hashrate = self.max_hashrate.max(other.max_hashrate);
        self.sum_miners += other.sum_miners;
        self.max_miners = self.max_miners.max(other.max_miners);
        self.n += other.n;
    }
    fn avg_hashrate(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum_hashrate / self.n as f64 }
    }
    fn avg_miners(&self) -> u32 {
        if self.n == 0 { 0 } else { (self.sum_miners / self.n as f64).round() as u32 }
    }
}

/// Which retention tier(s) a query should read from — chosen per requested
/// range rather than dynamically blended, so behavior stays simple and
/// predictable (see module doc for the reasoning: 24h always reads 1-minute
/// raw points; 7d/30d always read hourly averages; 1y/all always read daily
/// averages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRange {
    Day,
    Week,
    Month,
    Year,
    All,
}

impl HistoryRange {
    pub fn parse(s: &str) -> Self {
        match s {
            "7d" => HistoryRange::Week,
            "30d" => HistoryRange::Month,
            "1y" => HistoryRange::Year,
            "all" => HistoryRange::All,
            // "24h" and anything unrecognized default to the original
            // Quillon-modal behavior: a 24h window.
            _ => HistoryRange::Day,
        }
    }

    fn window_secs(self) -> Option<u64> {
        match self {
            HistoryRange::Day => Some(24 * 3600),
            HistoryRange::Week => Some(7 * DAY_SECS),
            HistoryRange::Month => Some(30 * DAY_SECS),
            HistoryRange::Year => Some(365 * DAY_SECS),
            HistoryRange::All => None,
        }
    }
}

pub struct MiningHistoryStore {
    db: flux_db::Database,
    last_compaction: AtomicU64,
}

impl MiningHistoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let db = flux_db::Database::open(path.as_ref().to_path_buf())?;
        Ok(Self { db, last_compaction: AtomicU64::new(0) })
    }

    /// A disk-backed store at a uniquely-named temp directory — for tests
    /// and `AppState::new()` (the constructor used by unit tests elsewhere
    /// in this crate, which has no real snapshot directory to hand in).
    pub fn open_ephemeral() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sigil-mining-history-ephemeral-{nanos}-{n}-{}", std::process::id()));
        Self::open(dir).expect("open ephemeral mining history store")
    }

    /// Record one live sample (from `MiningBridge::stats()`) and, at most
    /// once every [`COMPACTION_INTERVAL_SECS`], fold aged-out rows into the
    /// next coarser tier.
    pub fn record_sample(&self, now_secs: u64, hashrate: f64, miners: u32) -> Result<(), String> {
        let point = HistoryPoint { timestamp: now_secs, hashrate, miners };
        let bytes = serde_json::to_vec(&point).map_err(|e| e.to_string())?;
        self.db.put(&key(PFX_RAW, now_secs), &bytes)?;

        let last = self.last_compaction.load(Ordering::Relaxed);
        if now_secs.saturating_sub(last) >= COMPACTION_INTERVAL_SECS {
            self.last_compaction.store(now_secs, Ordering::Relaxed);
            self.compact_raw_to_hourly(now_secs)?;
            self.compact_hourly_to_daily(now_secs)?;
        }
        Ok(())
    }

    /// Fold every raw sample older than [`RAW_RETENTION_SECS`] into hourly
    /// buckets, then delete the consumed raw rows. Idempotent: rerunning
    /// with the same `now_secs` (or a slightly later one) is a correct
    /// no-op once nothing is left to fold.
    fn compact_raw_to_hourly(&self, now_secs: u64) -> Result<(), String> {
        let cutoff = now_secs.saturating_sub(RAW_RETENTION_SECS);
        let mut buckets: std::collections::HashMap<u64, Agg> = std::collections::HashMap::new();
        let mut consumed_keys: Vec<Vec<u8>> = Vec::new();

        for (k, v) in self.db.iter_from(&key_range_start(PFX_RAW)) {
            if k.first() != Some(&PFX_RAW) { break; }
            if k.len() != 9 { continue; }
            let ts = u64::from_be_bytes(k[1..9].try_into().unwrap());
            if ts >= cutoff { break; } // keys are sorted ascending by ts within the prefix
            let Ok(point) = serde_json::from_slice::<HistoryPoint>(&v) else { continue };
            let bucket = ts - (ts % HOUR_SECS);
            buckets.entry(bucket).or_insert_with(Agg::default).merge(&Agg::from_point(point.hashrate, point.miners));
            consumed_keys.push(k);
        }
        if buckets.is_empty() {
            return Ok(());
        }
        for (bucket, agg) in buckets {
            self.merge_into_bucket(PFX_HOURLY, bucket, agg)?;
        }
        for k in consumed_keys {
            self.db.delete(&k)?;
        }
        Ok(())
    }

    /// Same shape as [`compact_raw_to_hourly`], one tier coarser.
    fn compact_hourly_to_daily(&self, now_secs: u64) -> Result<(), String> {
        let cutoff = now_secs.saturating_sub(HOURLY_RETENTION_SECS);
        let mut buckets: std::collections::HashMap<u64, Agg> = std::collections::HashMap::new();
        let mut consumed_keys: Vec<Vec<u8>> = Vec::new();

        for (k, v) in self.db.iter_from(&key_range_start(PFX_HOURLY)) {
            if k.first() != Some(&PFX_HOURLY) { break; }
            if k.len() != 9 { continue; }
            let ts = u64::from_be_bytes(k[1..9].try_into().unwrap());
            if ts >= cutoff { break; }
            let Ok(agg) = serde_json::from_slice::<Agg>(&v) else { continue };
            let bucket = ts - (ts % DAY_SECS);
            buckets.entry(bucket).or_insert_with(Agg::default).merge(&agg);
            consumed_keys.push(k);
        }
        if buckets.is_empty() {
            return Ok(());
        }
        for (bucket, agg) in buckets {
            self.merge_into_bucket(PFX_DAILY, bucket, agg)?;
        }
        for k in consumed_keys {
            self.db.delete(&k)?;
        }
        Ok(())
    }

    fn merge_into_bucket(&self, prefix: u8, bucket: u64, incoming: Agg) -> Result<(), String> {
        let k = key(prefix, bucket);
        let mut agg = match self.db.get(&k)? {
            Some(bytes) => serde_json::from_slice::<Agg>(&bytes).unwrap_or_default(),
            None => Agg::default(),
        };
        agg.merge(&incoming);
        let bytes = serde_json::to_vec(&agg).map_err(|e| e.to_string())?;
        self.db.put(&k, &bytes)
    }

    /// Force a compaction pass regardless of the interval gate — for tests,
    /// so a seeded fixture doesn't have to wait out real wall-clock time.
    pub fn force_compact(&self, now_secs: u64) -> Result<(), String> {
        self.last_compaction.store(now_secs, Ordering::Relaxed);
        self.compact_raw_to_hourly(now_secs)?;
        self.compact_hourly_to_daily(now_secs)?;
        Ok(())
    }

    /// Serve a query for the given range as of `now_secs`. See
    /// [`HistoryRange`] for which tier backs which range.
    pub fn query_range(&self, range: HistoryRange, now_secs: u64) -> Vec<HistoryPoint> {
        let since = range.window_secs().map(|w| now_secs.saturating_sub(w)).unwrap_or(0);
        let points = match range {
            HistoryRange::Day => self.scan_raw(since),
            HistoryRange::Week | HistoryRange::Month => self.scan_rollup(PFX_HOURLY, since),
            HistoryRange::Year | HistoryRange::All => self.scan_rollup(PFX_DAILY, since),
        };
        points
    }

    fn scan_raw(&self, since: u64) -> Vec<HistoryPoint> {
        let mut out = Vec::new();
        for (k, v) in self.db.iter_from(&key_range_start(PFX_RAW)) {
            if k.first() != Some(&PFX_RAW) { break; }
            if k.len() != 9 { continue; }
            let ts = u64::from_be_bytes(k[1..9].try_into().unwrap());
            if ts < since { continue; }
            if let Ok(point) = serde_json::from_slice::<HistoryPoint>(&v) {
                out.push(point);
            }
        }
        out
    }

    fn scan_rollup(&self, prefix: u8, since: u64) -> Vec<HistoryPoint> {
        let mut out = Vec::new();
        for (k, v) in self.db.iter_from(&key_range_start(prefix)) {
            if k.first() != Some(&prefix) { break; }
            if k.len() != 9 { continue; }
            let ts = u64::from_be_bytes(k[1..9].try_into().unwrap());
            if ts < since { continue; }
            if let Ok(agg) = serde_json::from_slice::<Agg>(&v) {
                out.push(HistoryPoint { timestamp: ts, hashrate: agg.avg_hashrate(), miners: agg.avg_miners() });
            }
        }
        out
    }
}

/// Spawn the periodic sampler: every `every` (60s in production), pulls the
/// live aggregate off `mining` and records one durable sample. Mirrors
/// `sigil-node::search_index::spawn_indexer`'s shape — a self-contained
/// background task that only ever READS the live bridge, never touches
/// block-application/consensus code.
pub fn spawn_sampler(
    mining: Arc<super::mining::MiningBridge>,
    store: Arc<MiningHistoryStore>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let now_ms = now_secs.saturating_mul(1000);
            let (net_hps, live_miners, _blocks, _shares, _rejects) = mining.stats(now_ms);
            if let Err(e) = store.record_sample(now_secs, net_hps, live_miners as u32) {
                eprintln!("⚠ mining history sample failed: {e}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MiningHistoryStore {
        MiningHistoryStore::open_ephemeral()
    }

    #[test]
    fn raw_samples_round_trip_within_24h_window() {
        let s = store();
        let base = 1_000_000_000u64;
        for i in 0..10u64 {
            s.record_sample(base + i * 60, 100.0 + i as f64, 5).unwrap();
        }
        let day = s.query_range(HistoryRange::Day, base + 9 * 60);
        assert_eq!(day.len(), 10, "all 10 samples are within the 24h window");
        assert_eq!(day[0].timestamp, base);
        assert!((day[0].hashrate - 100.0).abs() < 1e-9);
        assert_eq!(day[0].miners, 5);
    }

    #[test]
    fn raw_older_than_48h_rolls_up_into_hourly_and_is_deleted() {
        let s = store();
        let base = 2_000_000_000u64;
        // Seed a run of raw samples spanning the raw retention boundary:
        // some inside 48h (kept raw), some well outside it (must roll up).
        // Two full hours' worth of once-a-minute samples, both older than
        // 48h relative to `now`.
        let now = base + 90 * 3600; // "now" is 90h after base -> base is 42h older than the 48h cutoff
        let bucket0 = base - (base % HOUR_SECS);
        for i in 0..60u64 {
            s.record_sample(bucket0 + i * 60, 200.0, 4).unwrap();
        }
        let bucket1 = bucket0 + HOUR_SECS;
        for i in 0..60u64 {
            s.record_sample(bucket1 + i * 60, 400.0, 8).unwrap();
        }
        s.force_compact(now).unwrap();

        // Raw tier for that span must now be empty.
        let raw = s.scan_raw(0);
        assert!(raw.iter().all(|p| p.timestamp >= now.saturating_sub(RAW_RETENTION_SECS)),
            "raw rows older than the retention window must have been deleted by compaction");

        let hourly = s.query_range(HistoryRange::Week, now);
        let b0 = hourly.iter().find(|p| p.timestamp == bucket0).expect("hourly bucket 0 present");
        assert!((b0.hashrate - 200.0).abs() < 1e-6, "bucket 0 average must be exactly 200 (constant input)");
        assert_eq!(b0.miners, 4);
        let b1 = hourly.iter().find(|p| p.timestamp == bucket1).expect("hourly bucket 1 present");
        assert!((b1.hashrate - 400.0).abs() < 1e-6);
        assert_eq!(b1.miners, 8);
    }

    #[test]
    fn hourly_older_than_60d_rolls_up_into_daily_and_is_deleted() {
        let s = store();
        let day0 = 3_000_000_000u64 - (3_000_000_000u64 % DAY_SECS);
        // Seed 24 hourly buckets covering exactly one day, with varying
        // hashrate so avg != max is actually exercised.
        for h in 0..24u64 {
            let ts = day0 + h * HOUR_SECS;
            let agg = Agg::from_point(if h == 12 { 1000.0 } else { 100.0 }, 10);
            s.merge_into_bucket(PFX_HOURLY, ts, agg).unwrap();
        }
        let now = day0 + 70 * DAY_SECS; // well past the 60-day hourly retention
        s.force_compact(now).unwrap();

        let hourly_left = s.scan_rollup(PFX_HOURLY, 0);
        assert!(hourly_left.iter().all(|p| p.timestamp >= now.saturating_sub(HOURLY_RETENTION_SECS)),
            "hourly rows older than the retention window must have been deleted by compaction");

        let daily = s.query_range(HistoryRange::All, now);
        let d0 = daily.iter().find(|p| p.timestamp == day0).expect("daily bucket present");
        // avg of 23x100 + 1x1000 over 24 samples = 137.5
        assert!((d0.hashrate - 137.5).abs() < 1e-6, "daily average must correctly weight all 24 hourly samples, got {}", d0.hashrate);
        assert_eq!(d0.miners, 10);
    }

    #[test]
    fn range_parse_matches_frontend_query_values() {
        assert!(matches!(HistoryRange::parse("24h"), HistoryRange::Day));
        assert!(matches!(HistoryRange::parse("bogus"), HistoryRange::Day));
        assert!(matches!(HistoryRange::parse("7d"), HistoryRange::Week));
        assert!(matches!(HistoryRange::parse("30d"), HistoryRange::Month));
        assert!(matches!(HistoryRange::parse("1y"), HistoryRange::Year));
        assert!(matches!(HistoryRange::parse("all"), HistoryRange::All));
    }

    #[test]
    fn all_time_range_has_no_window_floor() {
        let s = store();
        s.record_sample(0, 1.0, 1).unwrap(); // an extremely old sample
        s.force_compact(200_000_000).unwrap();
        // Not asserting content here (0 rolls all the way to daily under a
        // huge "now") -- just that `All` never filters by a since-cutoff,
        // unlike every other range.
        assert_eq!(HistoryRange::All.window_secs(), None);
        assert!(HistoryRange::Year.window_secs().is_some());
    }
}
