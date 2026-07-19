//! chronos_verify — presence audit for chronos_scale-shaped data.
//!
//! Samples N heights uniformly across [0, max_height), reconstructs the exact
//! chronos_scale key (b"blk/" ++ height BE ++ blake3(height LE)[..8]) and counts
//! hits vs misses, bucketed by height decile so loss PATTERNS are visible
//! (all-early-missing vs stripes vs random).
//!
//! Sharded stores are detected by their SHARDS marker and opened via
//! `ShardedDb::open_existing` (the count comes from disk — no env needed, and
//! a plain `Database::open` on a sharded root can never silently report false
//! absence). In sharded mode the audit reads in BATCHES through
//! `ShardedDb::get_many`, so every shard's read pipeline works in parallel
//! instead of one serial get() per sample — the difference between minutes
//! and seconds on a multi-TB audit.
//!
//!   chronos_verify <db-dir> <max_height> [samples=10000]

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: chronos_verify <db-dir> <max_height> [samples]");
    let maxh: u64 = args.next().expect("max_height").parse().expect("max_height u64");
    let n: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000);

    enum Vdb { One(flux_db::Database), Many(flux_db::shard::ShardedDb) }
    let db = if flux_db::shard::exists(std::path::Path::new(&dir)) {
        let s = flux_db::shard::ShardedDb::open_existing(&dir).expect("open sharded");
        eprintln!("chronos_verify: sharded store, {} shards (from SHARDS marker)", s.shard_count());
        Vdb::Many(s)
    } else {
        Vdb::One(flux_db::Database::open(&dir).expect("open"))
    };

    let key_for = |h: u64| -> Vec<u8> {
        let seed = blake3::hash(&h.to_le_bytes());
        let mut key = Vec::with_capacity(20);
        key.extend_from_slice(b"blk/");
        key.extend_from_slice(&h.to_be_bytes());
        key.extend_from_slice(&seed.as_bytes()[..8]);
        key
    };

    let step = (maxh / n).max(1);
    let mut found = 0u64;
    let mut total = 0u64;
    let mut decile_found = [0u64; 10];
    let mut decile_total = [0u64; 10];
    let mut first_missing: Option<u64> = None;
    let mut last_found: Option<u64> = None;

    // Heights ascend, so per-batch results processed in order keep the
    // first_missing / last_found semantics of the old serial loop.
    const BATCH: usize = 4096;
    let mut record = |h: u64, hit: bool,
                      found: &mut u64, total: &mut u64,
                      decile_found: &mut [u64; 10], decile_total: &mut [u64; 10],
                      first_missing: &mut Option<u64>, last_found: &mut Option<u64>| {
        let d = ((h as u128 * 10) / maxh as u128) as usize;
        decile_total[d] += 1;
        *total += 1;
        if hit {
            *found += 1;
            decile_found[d] += 1;
            *last_found = Some(h);
        } else if first_missing.is_none() {
            *first_missing = Some(h);
        }
    };

    let mut heights: Vec<u64> = Vec::with_capacity(BATCH);
    let mut h = 0u64;
    while h < maxh {
        heights.push(h);
        h += step;
        if heights.len() == BATCH || h >= maxh {
            match &db {
                Vdb::Many(d) => {
                    // One parallel batched read across all shard pipelines.
                    let keys: Vec<Vec<u8>> = heights.iter().map(|&hh| key_for(hh)).collect();
                    let got = d.get_many(&keys).expect("get_many");
                    for (&hh, v) in heights.iter().zip(got.iter()) {
                        record(hh, v.is_some(), &mut found, &mut total,
                            &mut decile_found, &mut decile_total,
                            &mut first_missing, &mut last_found);
                    }
                }
                Vdb::One(d) => {
                    for &hh in &heights {
                        let hit = d.get(&key_for(hh)).ok().flatten().is_some();
                        record(hh, hit, &mut found, &mut total,
                            &mut decile_found, &mut decile_total,
                            &mut first_missing, &mut last_found);
                    }
                }
            }
            heights.clear();
        }
    }

    println!("presence: {}/{} ({:.2}%)", found, total, found as f64 / total as f64 * 100.0);
    println!("first_missing={:?} last_found={:?}", first_missing, last_found);
    for d in 0..10 {
        println!("decile {} [{}..{}): {}/{}", d, d as u64 * maxh / 10, (d as u64 + 1) * maxh / 10,
            decile_found[d], decile_total[d]);
    }
}
