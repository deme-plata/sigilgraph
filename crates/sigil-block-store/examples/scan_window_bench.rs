//! Per-height reads vs one sequential pass, on a real store.
//!
//!   scan_window_bench <db-path> <start-height> [window=2000000] [per-height-sample=5000]
//!
//! Read-only. Point it at a COPY of a store, never at one a running node holds open.

use sigil_block_store::BlockStore;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: scan_window_bench <db-path> <start-height> [window] [per-height-sample]");
        std::process::exit(2);
    }
    let start: u64 = a[2].parse().expect("start height");
    let window: u64 = a.get(3).map(|s| s.parse().expect("window")).unwrap_or(2_000_000);
    let sample: u64 = a.get(4).map(|s| s.parse().expect("sample")).unwrap_or(5_000);

    let t = Instant::now();
    let store = BlockStore::open_blocking(&a[1]).expect("open store");
    println!("open {:.1} s · synced_to {} · verified_to {}", t.elapsed().as_secs_f64(), store.synced_to(), store.verified_to());

    // Old path: one height-index read + one body read per height.
    let t = Instant::now();
    let mut found = 0u64;
    for h in start..start + sample {
        if store.get_stored_at_height(h).is_some() {
            found += 1;
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!("per-height: {found}/{sample} blocks in {s:.1} s = {:.0} blk/s", sample as f64 / s);

    // New path: one sequential pass, mapping each block through the same precheck verify uses.
    let t = Instant::now();
    let w = store
        .scan_window(start + sample, window, |b| (b.header.precheck().is_ok(), b.header.parent_hash))
        .expect("scan");
    let s = t.elapsed().as_secs_f64();
    let got = w.slots.iter().filter(|x| x.is_some()).count();
    let ok = w.slots.iter().flatten().filter(|(p, _)| *p).count();
    println!(
        "sequential: {got}/{} blocks ({ok} precheck ok) in {s:.1} s = {:.0} blk/s · index {} ms · scan {} ms · {} SSTs ({} vanished) · {:.1} M pairs",
        w.indexed,
        w.indexed as f64 / s,
        w.index_ms,
        w.scan_ms,
        w.raw.files,
        w.raw.vanished,
        w.raw.pairs as f64 / 1e6
    );
}
