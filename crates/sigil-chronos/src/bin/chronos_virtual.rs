//! chronos_virtual — the storage curve in VIRTUAL TIME.
//!
//! Mirrors flux-db's STRUCTURAL evolution exactly (WAL fills to 64 MiB ->
//! flush -> L0 SST; L0_COMPACT_THRESHOLD=4 -> leveled 4-ary cascade L0->L1->L2
//! -> the SST population is a base-4 counter over total flushes) and prices
//! every window with a COST MODEL least-squares-fitted from a real run's
//! metrics.csv (the physical chronos_scale run is the calibration set).
//!
//! Same CSV schema as chronos_scale, so curves overlay directly. Rows carry
//! a trailing `extrapolated` flag once the model leaves the calibrated range —
//! virtual time never pretends interpolation where it is extrapolating.
//!
//!   CHRONOS_CALIB         (default /home/storage/chronos-5tb/metrics.csv)
//!   CHRONOS_TARGET_BYTES  (default 5 TiB)
//!   CHRONOS_VALUE_BYTES   (default 8192 — must match the calibration run)
//!   CHRONOS_SAMPLE_EVERY  (default 100_000)
//!   CHRONOS_OUT           (default /home/storage/chronos-virtual/metrics-virtual.csv)

use std::io::Write as _;

// flux-db structural constants (crates/flux-db/src/lib.rs)
const MAX_WAL: u64 = 64 * 1024 * 1024;      // DEFAULT_MAX_WAL_BYTES
const L_FANOUT: u64 = 4;                    // L0_COMPACT_THRESHOLD (4-ary cascade)
const WAL_ENTRY_OVERHEAD: u64 = 12;         // [crc][klen][vlen]
const KEY_BYTES: u64 = 20;                  // "blk/" + be64 + 8 seed bytes

struct Fit { a: f64, b: f64 } // y = a + b*x
fn fit(xs: &[f64], ys: &[f64]) -> Fit {
    let n = xs.len() as f64;
    if n < 2.0 { return Fit { a: ys.first().copied().unwrap_or(0.0), b: 0.0 }; }
    let (sx, sy): (f64, f64) = (xs.iter().sum(), ys.iter().sum());
    let sxx: f64 = xs.iter().map(|x| x * x).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| x * y).sum();
    let d = n * sxx - sx * sx;
    if d.abs() < 1e-9 { return Fit { a: sy / n, b: 0.0 }; }
    let b = (n * sxy - sx * sy) / d;
    Fit { a: (sy - b * sx) / n, b }
}
impl Fit { fn at(&self, x: f64) -> f64 { self.a + self.b * x } }

fn main() {
    let calib = std::env::var("CHRONOS_CALIB")
        .unwrap_or_else(|_| "/home/storage/chronos-5tb/metrics.csv".into());
    let target: u64 = std::env::var("CHRONOS_TARGET_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(5 * 1024 * 1024 * 1024 * 1024);
    let value_bytes: u64 = std::env::var("CHRONOS_VALUE_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(8192);
    let sample_every: u64 = std::env::var("CHRONOS_SAMPLE_EVERY").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let out = std::env::var("CHRONOS_OUT")
        .unwrap_or_else(|_| "/home/storage/chronos-virtual/metrics-virtual.csv".into());

    // ── calibration: parse the real run's CSV ────────────────────────────
    let text = std::fs::read_to_string(&calib).expect("calibration csv");
    let (mut gb, mut rate, mut ssts_x, mut p50s, mut p99s, mut bytes_per_blk) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 9 { continue; }
        let (h, bps, dirb, sst, p50, p99): (f64, f64, f64, f64, f64, f64) = (
            f[0].parse().unwrap_or(0.0), f[2].parse().unwrap_or(0.0),
            f[4].parse().unwrap_or(0.0), f[6].parse().unwrap_or(0.0),
            f[7].parse().unwrap_or(0.0), f[8].parse().unwrap_or(0.0));
        if h <= 0.0 || bps <= 0.0 { continue; }
        gb.push(dirb / 1e9); rate.push(bps);
        ssts_x.push(sst); p50s.push(p50); p99s.push(p99);
        bytes_per_blk.push(dirb / h);
    }
    let n = gb.len();
    assert!(n >= 3, "need >=3 calibration samples, got {n}");
    let rate_fit = fit(&gb, &rate);             // blk/s vs store-GB (write decay)
    let p50_fit = fit(&ssts_x, &p50s);          // read p50 vs SST count
    let p99_fit = fit(&ssts_x, &p99s);          // read p99 vs SST count
    let bpb = bytes_per_blk.iter().sum::<f64>() / n as f64; // real on-disk bytes/block (incl index overhead)
    let calib_max_gb = gb.last().copied().unwrap_or(0.0);
    let calib_max_sst = ssts_x.iter().cloned().fold(0.0, f64::max);
    eprintln!("calibrated from {n} samples (to {calib_max_gb:.1} GB): rate = {:.0} {:+.3}*GB blk/s · p99 = {:.0} {:+.1}*ssts us · {bpb:.0} B/blk on disk",
        rate_fit.a, rate_fit.b, p99_fit.a, p99_fit.b);

    // ── virtual evolution ────────────────────────────────────────────────
    std::fs::create_dir_all(std::path::Path::new(&out).parent().unwrap()).unwrap();
    let mut csv = std::fs::File::create(&out).expect("out csv");
    let _ = writeln!(csv, "height,elapsed_s,blk_per_s,mb_per_s,dir_bytes,wal_bytes,sst_count,read_p50_us,read_p99_us,extrapolated");

    let entry = WAL_ENTRY_OVERHEAD + KEY_BYTES + value_bytes; // WAL bytes per put
    let entries_per_flush = MAX_WAL / entry + 1;              // flush when wal EXCEEDS cap
    let t_wall = std::time::Instant::now();
    let (mut height, mut virt_secs) = (0u64, 0f64);
    loop {
        height += sample_every;
        let dir_bytes = (height as f64 * bpb) as u64;
        let gb_now = dir_bytes as f64 / 1e9;
        let wal_bytes = (height % entries_per_flush) * entry;
        // SST population = base-4 counter over total flushes (L0->L1->L2->…)
        let mut flushes = height / entries_per_flush;
        let mut sst_count = 0u64;
        while flushes > 0 { sst_count += flushes % L_FANOUT; flushes /= L_FANOUT; }
        let bps = rate_fit.at(gb_now).max(rate_fit.a * 0.05); // floor: never below 5% of t0-rate
        virt_secs += sample_every as f64 / bps;
        let p50 = p50_fit.at(sst_count as f64).max(1.0);
        let p99 = p99_fit.at(sst_count as f64).max(p50);
        let extrap = (gb_now > calib_max_gb) || (sst_count as f64 > calib_max_sst);
        let _ = writeln!(csv, "{},{:.0},{:.0},{:.1},{},{},{},{:.0},{:.0},{}",
            height, virt_secs, bps, bps * value_bytes as f64 / (1024.0 * 1024.0),
            dir_bytes, wal_bytes, sst_count, p50, p99, extrap as u8);
        if dir_bytes >= target {
            eprintln!("VIRTUAL TARGET: {:.2} TB at height {} — virtual elapsed {:.1} h, wall-clock {:?}",
                dir_bytes as f64 / 1e12, height, virt_secs / 3600.0, t_wall.elapsed());
            break;
        }
    }
}
