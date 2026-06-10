//! chronos_scale — the 5 TB STORAGE-CURVE chronos test.
//!
//! Writes deterministic chronos-style blocks (header-derived key + a padded
//! tx payload) through the REAL store engine (flux-db: WAL -> memtable -> SST
//! flush cycles) until the on-disk store reaches a target size, sampling the
//! rate/latency curve along the way. The point is not a benchmark number but
//! the SHAPE: where write throughput sags, how WAL cycling behaves at scale,
//! and what read-back latency does as the store grows — "find better ways".
//!
//!   CHRONOS_DIR    (default /home/storage/chronos-5tb/db)
//!   CHRONOS_TARGET_BYTES (default 5 TB)
//!   CHRONOS_VALUE_BYTES  (default 8192)
//!   CHRONOS_SAMPLE_EVERY (default 100_000 blocks)
//!
//! Metrics CSV: <CHRONOS_DIR>/../metrics.csv
//!   height,elapsed_s,blk_per_s,mb_per_s,dir_bytes,wal_bytes,sst_count,read_p50_us,read_p99_us

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn dir_stats(dir: &Path) -> (u64, u64, u64) {
    // (total_bytes, wal_bytes, sst_count)
    let (mut total, mut wal, mut ssts) = (0u64, 0u64, 0u64);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            total += len;
            let name = e.file_name().to_string_lossy().to_string();
            if name == "flux.wal" { wal = len; }
            if name.ends_with(".sst") { ssts += 1; }
        }
    }
    (total, wal, ssts)
}

fn main() {
    let dir: PathBuf = std::env::var("CHRONOS_DIR")
        .unwrap_or_else(|_| "/home/storage/chronos-5tb/db".into()).into();
    let target: u64 = std::env::var("CHRONOS_TARGET_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(5 * 1024 * 1024 * 1024 * 1024);
    let value_bytes: usize = std::env::var("CHRONOS_VALUE_BYTES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(8192);
    let sample_every: u64 = std::env::var("CHRONOS_SAMPLE_EVERY").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(100_000);

    std::fs::create_dir_all(&dir).expect("create dir");
    let csv_path = dir.parent().unwrap().join("metrics.csv");
    let mut csv = std::fs::OpenOptions::new().create(true).append(true)
        .open(&csv_path).expect("csv");
    if csv.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = writeln!(csv, "height,elapsed_s,blk_per_s,mb_per_s,dir_bytes,wal_bytes,sst_count,read_p50_us,read_p99_us");
    }

    let db = flux_db::Database::open(&dir).expect("open flux-db");
    eprintln!("chronos_scale: dir={:?} target={} GiB value={} B sample_every={}",
        dir, target / (1024*1024*1024), value_bytes, sample_every);

    // deterministic chronos payload: blake3 keystream over the height — incompressible,
    // reproducible, and the key embeds a header-style (height, hash) identity.
    let mut height: u64 = {
        // resume: continue from the persisted height marker if present (re-runnable).
        std::fs::read_to_string(dir.parent().unwrap().join("height.marker"))
            .ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    };
    let t0 = Instant::now();
    let mut window_t = Instant::now();
    let mut window_blocks = 0u64;

    loop {
        // build the deterministic block value
        let mut value = Vec::with_capacity(value_bytes);
        let seed = blake3::hash(&height.to_le_bytes());
        let mut counter = 0u64;
        while value.len() < value_bytes {
            let mut h = blake3::Hasher::new();
            h.update(seed.as_bytes());
            h.update(&counter.to_le_bytes());
            value.extend_from_slice(h.finalize().as_bytes());
            counter += 1;
        }
        value.truncate(value_bytes);
        let key = {
            let mut k = Vec::with_capacity(40);
            k.extend_from_slice(b"blk/");
            k.extend_from_slice(&height.to_be_bytes());
            k.extend_from_slice(&seed.as_bytes()[..8]);
            k
        };
        db.put(&key, &value).expect("put");
        height += 1;
        window_blocks += 1;

        if height % sample_every == 0 {
            let (total, wal, ssts) = dir_stats(&dir);
            // read-back probe: 64 random gets across the whole range
            let mut lat: Vec<u128> = Vec::with_capacity(64);
            for i in 0..64u64 {
                let probe_h = (blake3::hash(&(height ^ i).to_le_bytes()).as_bytes()[0] as u64)
                    .wrapping_mul(height / 256).min(height.saturating_sub(1));
                let pseed = blake3::hash(&probe_h.to_le_bytes());
                let mut pk = Vec::with_capacity(40);
                pk.extend_from_slice(b"blk/");
                pk.extend_from_slice(&probe_h.to_be_bytes());
                pk.extend_from_slice(&pseed.as_bytes()[..8]);
                let t = Instant::now();
                let _ = db.get(&pk);
                lat.push(t.elapsed().as_micros());
            }
            lat.sort_unstable();
            let (p50, p99) = (lat[31], lat[62]);
            let wsecs = window_t.elapsed().as_secs_f64().max(1e-9);
            let bps = window_blocks as f64 / wsecs;
            let mbs = bps * value_bytes as f64 / (1024.0 * 1024.0);
            let line = format!("{},{:.0},{:.0},{:.1},{},{},{},{},{}",
                height, t0.elapsed().as_secs_f64(), bps, mbs, total, wal, ssts, p50, p99);
            let _ = writeln!(csv, "{line}");
            let _ = csv.flush();
            let _ = std::fs::write(dir.parent().unwrap().join("height.marker"), height.to_string());
            eprintln!("[scale] {line}");
            window_t = Instant::now();
            window_blocks = 0;
            if total >= target {
                eprintln!("chronos_scale: TARGET REACHED — {} bytes at height {}", total, height);
                break;
            }
        }
    }
}
