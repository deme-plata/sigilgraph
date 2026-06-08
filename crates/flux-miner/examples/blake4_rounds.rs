//! blake4_rounds — measure BLAKE4 hashrate vs round count.
//!
//! The "83× headroom" between BLAKE4-sound (full BLAKE3, ~155 MH/s) and the
//! invertible turbo ceiling (~12.9 GH/s) is what reduced rounds capture *while
//! staying a real hash*. This bench walks R = 7 → 1 and reports the speedup, so
//! the flux-development loop can pick a round count that is both fast AND has
//! enough preimage margin to deploy.
//!
//!   fluxc run --example blake4_rounds   (or build + run the example binary)

use flux_miner::pow::{blake4_word, FULL_ROUNDS};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const HEADER: [u8; 32] = [0x5a; 32];

fn mine(secs: f64, threads: usize, rounds: u32) -> u64 {
    let stop = AtomicBool::new(false);
    let total = AtomicU64::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let (stop, total) = (&stop, &total);
            s.spawn(move || {
                let mut nonce = (t as u64) << 40; // disjoint lane per thread
                let mut local = 0u64;
                loop {
                    for _ in 0..4096 {
                        let _ = blake4_word(&HEADER, nonce, rounds);
                        nonce += 1;
                    }
                    local += 4096;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
                total.fetch_add(local, Ordering::Relaxed);
            });
        }
        while t0.elapsed().as_secs_f64() < secs {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
    });
    total.load(Ordering::Relaxed)
}

fn fmt(hps: f64) -> String {
    if hps >= 1e9 {
        format!("{:.2} GH/s", hps / 1e9)
    } else if hps >= 1e6 {
        format!("{:.2} MH/s", hps / 1e6)
    } else {
        format!("{:.0} H/s", hps)
    }
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let secs = 2.0;
    println!("\n  BLAKE4 — hashrate vs round count  [{cores} cores, {secs}s/sample]\n");
    println!("  {:>6}  {:>14}  {:>9}   note", "rounds", "hashrate", "vs R=7");
    println!("  {}", "-".repeat(58));
    let base = (mine(secs, cores, FULL_ROUNDS) as f64 / secs).max(1.0);
    for r in [7u32, 6, 5, 4, 3, 2, 1] {
        let hps = mine(secs, cores, r) as f64 / secs;
        let note = if r == FULL_ROUNDS {
            "BLAKE3 (sound anchor, KAT-verified)"
        } else {
            "reduced — thinner preimage margin"
        };
        println!("  {:>6}  {:>14}  {:>8.2}×   {note}", r, fmt(hps), hps / base);
    }
    println!("\n  Reduced rounds are a real speedup; how low R can go while staying");
    println!("  preimage-hard enough to deploy is the open BLAKE4 question.\n");
}
