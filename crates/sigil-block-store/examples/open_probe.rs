//! Time a real `BlockStore` open and show where it spends the time.
//!
//! `cargo run --example open_probe -- <store path>` opens the store the way sigil-top's
//! dashboard does (`open`, background index migration), prints a heartbeat every 10 s
//! with the PID (so `gdb -p <pid> -batch -ex 'thread apply all bt'` can sample the silent
//! phases), then the watermarks and the total time. Point it at a COPY of a store: an
//! open writes (WAL, the synced watermark).
use sigil_block_store::BlockStore;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).expect("usage: open_probe <store path> [rewind-synced N]");
    // `rewind-synced N`: move the persisted synced watermark (meta 'S') back by N first, to
    // time the open's contiguous-pointer walk (`advance_synced`) over N stored heights.
    if let Some(n) = std::env::args().nth(2).and_then(|a| a.parse::<u64>().ok()) {
        let db = flux_db::Database::open(std::path::PathBuf::from(&path)).expect("open for rewind");
        let s = db
            .get(&[0x02, b'S'])
            .ok()
            .flatten()
            .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok().map(u64::from_be_bytes))
            .unwrap_or(0);
        let to = s.saturating_sub(n);
        db.put(&[0x02, b'S'], &to.to_be_bytes()).expect("put S");
        db.flush().ok();
        println!("rewound synced {s} -> {to}");
        if std::env::args().nth(3).as_deref() == Some("only") {
            return;
        }
    }
    println!("pid {} opening {path}", std::process::id());
    let t0 = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.clone();
    std::thread::Builder::new()
        .name("open-probe".into())
        .spawn(move || {
            let _ = tx.send(BlockStore::open(&p));
        })
        .expect("spawn");
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(s)) => {
                println!(
                    "opened in {:.1}s best={} synced={} verified={}",
                    t0.elapsed().as_secs_f64(),
                    s.best_height(),
                    s.synced_to(),
                    s.verified_to()
                );
                return;
            }
            Ok(Err(e)) => {
                println!("open failed after {:.1}s: {e}", t0.elapsed().as_secs_f64());
                std::process::exit(1);
            }
            Err(_) => println!("  still opening… {:.0}s", t0.elapsed().as_secs_f64()),
        }
    }
}
