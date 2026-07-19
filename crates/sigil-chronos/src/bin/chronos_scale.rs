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
//! flux-db tuning knobs (all optional; unset = engine defaults, i.e. the historical baseline):
//!   CHRONOS_WAL_MAX_BYTES     max WAL size before flush cycle (Database::set_max_wal_bytes)
//!   CHRONOS_BLOCK_CACHE_BYTES read block-cache capacity (Database::with_block_cache_capacity)
//!   CHRONOS_DEFER_COMPACTION  1 = defer compaction during the write flood (set_defer_compaction)
//!   CHRONOS_SYNC_EVERY        fsync the WAL every N blocks (0/unset = engine default cadence)
//!   CHRONOS_SHARDS            ShardedDb shard count (default 1 = single store; measured
//!                             idle-array: 1=171 MB/s, 4=441, 8=496 vs 646 raw — flux-db
//!                             SHARDED-WRITER follow-up, audits 100.00% at every width)
//!   CHRONOS_BATCH             put_many batch size (default 256; 1 = legacy per-entry put()
//!                             path, kept for A/B rate benching). Measured on 8 KB blocks:
//!                             put()=~168 MB/s ceiling vs put_many coalesced-WAL batching
//!                             (one write+flush syscall per batch, lock taken once).
//!
//! Metrics CSV: <CHRONOS_DIR>/../metrics.csv
//!   height,elapsed_s,blk_per_s,mb_per_s,dir_bytes,wal_bytes,sst_count,read_p50_us,read_p99_us

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn dir_stats(dir: &Path) -> (u64, u64, u64) {
    // (total_bytes, wal_bytes, sst_count). Recurses ONE level so sharded
    // stores (data in shard-000..shard-NNN subdirs) are measured too — the
    // old flat read_dir saw ~0 bytes in sharded mode, so CHRONOS_TARGET_BYTES
    // never fired and the wal/sst CSV columns were wrong. `wal` is the SUM of
    // every shard's flux.wal (single-store: the one top-level WAL, unchanged).
    fn scan(dir: &Path, depth: u8, total: &mut u64, wal: &mut u64, ssts: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
                if md.is_dir() {
                    if depth > 0 { scan(&e.path(), depth - 1, total, wal, ssts); }
                    continue;
                }
                *total += md.len();
                let name = e.file_name().to_string_lossy().to_string();
                if name == "flux.wal" { *wal += md.len(); }
                if name.ends_with(".sst") { *ssts += 1; }
            }
        }
    }
    let (mut total, mut wal, mut ssts) = (0u64, 0u64, 0u64);
    scan(dir, 1, &mut total, &mut wal, &mut ssts);
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

    let knob = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    let (wal_max, block_cache, defer_comp, sync_every) = (
        knob("CHRONOS_WAL_MAX_BYTES"),
        knob("CHRONOS_BLOCK_CACHE_BYTES"),
        knob("CHRONOS_DEFER_COMPACTION").map(|v| v == 1).unwrap_or(false),
        knob("CHRONOS_SYNC_EVERY").unwrap_or(0),
    );
    let batch_size = knob("CHRONOS_BATCH").unwrap_or(256).max(1) as usize;
    // CHRONOS_SHARDS unset on an EXISTING sharded store must adopt the
    // persisted count, not default to 1 — Database::open(root) on a sharded
    // root would silently start a NEW single store beside the shards and
    // resume writing at the marker height into it (same marker auto-detect
    // chronos_verify uses; an explicit mismatching env still refuses loudly
    // inside ShardedDb::open).
    let shards = match knob("CHRONOS_SHARDS") {
        Some(n) => n.max(1) as usize,
        None if flux_db::shard::exists(&dir) => {
            let n: usize = std::fs::read_to_string(dir.join("SHARDS")).ok()
                .and_then(|s| s.trim().parse().ok()).unwrap_or(1);
            eprintln!("chronos_scale: existing sharded store — adopting {} shards from SHARDS marker", n);
            n
        }
        None => 1,
    };
    // Store facade: 1 shard = the single Database (byte-identical legacy path);
    // >1 = flux_db::shard::ShardedDb (N parallel WAL/memtable pipelines).
    enum Store { One(flux_db::Database), Many(flux_db::shard::ShardedDb) }
    impl Store {
        fn put(&self, k: &[u8], v: &[u8]) -> Result<(), String> {
            match self { Store::One(d) => d.put(k, v), Store::Many(d) => d.put(k, v) }
        }
        fn put_many(&self, e: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
            match self { Store::One(d) => d.put_many(e), Store::Many(d) => d.put_many(e) }
        }
        fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, String> {
            match self { Store::One(d) => d.get(k), Store::Many(d) => d.get(k) }
        }
        fn sync_wal(&self) -> Result<(), String> {
            match self { Store::One(d) => d.sync_wal(), Store::Many(d) => d.sync_wal() }
        }
    }
    let db = if shards > 1 {
        if block_cache.is_some() {
            eprintln!("chronos_scale: CHRONOS_BLOCK_CACHE_BYTES is per-Database; not applied in sharded mode");
        }
        let d = flux_db::shard::ShardedDb::open(&dir, shards).expect("open sharded flux-db");
        if let Some(w) = wal_max { d.set_max_wal_bytes(w); }
        if defer_comp { d.set_defer_compaction(true); }
        Store::Many(d)
    } else {
        let mut d = flux_db::Database::open(&dir).expect("open flux-db");
        if let Some(bc) = block_cache { d = d.with_block_cache_capacity(bc as usize); }
        if let Some(w) = wal_max { d.set_max_wal_bytes(w); }
        if defer_comp { d.set_defer_compaction(true); }
        Store::One(d)
    };
    eprintln!("chronos_scale: dir={:?} target={} GiB value={} B sample_every={}",
        dir, target / (1024*1024*1024), value_bytes, sample_every);
    eprintln!("chronos_scale: knobs wal_max={:?} block_cache={:?} defer_compaction={} sync_every={} batch={} shards={}",
        wal_max, block_cache, defer_comp, sync_every, batch_size, shards);
    let mut pending: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(batch_size);

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
        if batch_size <= 1 {
            db.put(&key, &value).expect("put");
        } else {
            pending.push((key, value));
            if pending.len() >= batch_size {
                db.put_many(&pending).expect("put_many");
                pending.clear();
            }
        }
        height += 1;
        window_blocks += 1;
        if sync_every > 0 && height % sync_every == 0 {
            if !pending.is_empty() { db.put_many(&pending).expect("put_many"); pending.clear(); }
            let _ = db.sync_wal();
        }

        if height % sample_every == 0 {
            // Drain the batch BEFORE probing/marking: the read probes must see
            // settled data, and the marker's durability barrier (sync_wal below)
            // must cover every height it claims.
            if !pending.is_empty() { db.put_many(&pending).expect("put_many"); pending.clear(); }
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
            // MARKER-AFTER-FSYNC (task #10): the marker is the audit's upper
            // bound — everything at or below it must be DURABLE. Buffered WAL
            // writes die with the process (kill -9 chaos measured ~5.5% loss
            // below the marker), so fsync the WAL BEFORE advancing the marker;
            // and write the marker atomically (tmp + fsync + rename) so a
            // crash mid-write can never leave a torn marker that would reset
            // a multi-TB run to height 0.
            match db.sync_wal() {
                Err(e) => eprintln!("[scale] sync_wal before marker FAILED ({e}) — marker not advanced"),
                Ok(()) => {
                    let marker = dir.parent().unwrap().join("height.marker");
                    let tmp_m = dir.parent().unwrap().join("height.marker.tmp");
                    let res = std::fs::write(&tmp_m, height.to_string())
                        .and_then(|_| std::fs::File::open(&tmp_m).and_then(|f| f.sync_all()))
                        .and_then(|_| std::fs::rename(&tmp_m, &marker));
                    if let Err(e) = res {
                        eprintln!("[scale] marker write failed: {e}");
                    }
                }
            }
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
